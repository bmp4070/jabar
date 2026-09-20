//! Server state and the main loop.
//!
//! One thread owns protocol state. Index reconciliation and cache writes run on
//! workers and send completed results back to that thread.
//!
//! What the loop guarantees now is the part that is painful to retrofit: every
//! request gets exactly one response, unknown methods are refused rather than
//! ignored, and a handler that panics does not take the session down.

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use build_model::{AspectConfig, AspectRunner, BazelCli};
use crossbeam_channel::{Receiver, Sender};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::notification::Notification as _;
use lsp_types::request::Request as _;
use paths::{AbsPath, AbsPathBuf};
use rustc_hash::FxHashSet;
use serde::Serialize;
use telemetry::Telemetry;
use vfs::{Vfs, VfsPath};

use crate::capabilities::{negotiate_encoding, server_capabilities, workspace_root};
use crate::config::Config;
use crate::documents::Documents;
use crate::handlers;
use crate::index_cache::{self, CacheKey};
use crate::line_index::PositionEncoding;
use crate::uri;
use overlay::Overlay;
use symbol_index::{ShardMetadata, SymbolIndex};
use watcher::{Change, FileWatcher};

/// Custom request: what the server currently believes about itself.
///
/// Not part of LSP. It exists because "is the language server working" is
/// otherwise unanswerable from outside, and for an agent client a server that is
/// quietly answering nothing looks exactly like a codebase with nothing in it.
pub const STATUS_REQUEST: &str = "jabar/status";

/// Custom request: like `textDocument/references`, but reports the true total.
///
/// LSP's own `references` returns a bare array, so a client receiving 200 of
/// 1,683 cannot tell it was truncated — on Gerrit, asking for references to
/// `Project` withholds 1,483 silently. An agent reads that as "this class has
/// 200 references" and acts on it.
///
/// The standard method stays conformant for clients that only speak LSP; this
/// one carries the count so a client that knows to ask gets the truth.
pub const REFERENCES_REQUEST: &str = "jabar/references";

/// Custom request: load SCIP shards from a directory into the global index.
///
/// Temporary. jabar will run the aspect itself once M4 lands; until then this
/// is how an index gets in.
pub const LOAD_INDEX_REQUEST: &str = "jabar/loadIndex";

/// Performs the `initialize` handshake and runs until the client disconnects.
pub fn run_server(connection: Connection) -> anyhow::Result<()> {
    let (id, params) = connection.initialize_start().context("initialize handshake failed")?;
    let params: lsp_types::InitializeParams =
        serde_json::from_value(params).context("client sent malformed InitializeParams")?;

    let encoding = negotiate_encoding(&params.capabilities);
    // Only send progress to a client that asked for it; an unsolicited
    // `$/progress` is noise at best and a protocol error at worst.
    let supports_progress =
        params.capabilities.window.as_ref().and_then(|w| w.work_done_progress).unwrap_or(false);
    let root = workspace_root(&params)
        .as_ref()
        .and_then(|url| uri::vfs_path(url).ok())
        .and_then(|path| path.as_real().cloned());

    if let Some(client) = &params.client_info {
        tracing::info!(name = %client.name, version = ?client.version, "client connected");
    }
    tracing::info!(?encoding, root = ?root.as_ref().map(|r| r.as_str()), "initializing");

    let config = Config::from_initialization_options(params.initialization_options.as_ref());
    tracing::debug!(?config, "configuration");

    // Find an index before advertising, because LSP has no way to say
    // "supported, but not yet": a provider advertised with nothing behind it
    // means clients call it and get nothing, which reads as "no such symbol".
    let mut discovered = root.as_deref().and_then(|root| discover_index(root, &config));

    // Building takes minutes and blocks the handshake, so it happens only when
    // the client asked for it and there is nothing to serve otherwise.
    if discovered.is_none()
        && config.index.auto
        && let Some(root) = root.as_deref()
    {
        match build_index(root, &config) {
            Ok(()) => discovered = discover_index(root, &config),
            Err(err) => tracing::warn!(%err, "could not build an index"),
        }
    }
    if let Some(discovered) = &discovered {
        tracing::info!(
            dir = %discovered.dir,
            cached = discovered.cache_hit,
            shards = discovered.index.shard_count(),
            definitions = discovered.index.definition_count(),
            "found an index at startup"
        );
    } else {
        tracing::info!(
            "no index found; run the SCIP aspect, then reopen or call `jabar/loadIndex`"
        );
    }

    let result = lsp_types::InitializeResult {
        capabilities: server_capabilities(encoding, discovered.is_some()),
        server_info: Some(lsp_types::ServerInfo {
            name: "jabar".to_owned(),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        }),
    };
    connection
        .initialize_finish(id, serde_json::to_value(result)?)
        .context("initialize handshake failed")?;

    let mut server = Server::new(connection.sender.clone(), encoding, root);
    server.supports_progress = supports_progress;
    server.apply_config(config);
    if let Some(discovered) = discovered {
        server.adopt_discovered(discovered);
    }
    server.run(&connection)
}

/// Runs the SCIP aspect, so a workspace with no index gets one.
///
/// Everything it needs beyond the workspace is discovered or configured;
/// missing pieces are reported rather than guessed, because a wrong `JAVA_HOME`
/// produces a confusing compiler error rather than an obvious one.
fn build_index(root: &AbsPath, config: &Config) -> Result<(), String> {
    let scip_java =
        config.resolve_scip_java().ok_or("`scip-java` is not on PATH; set `index.scipJava`")?;
    let java_home =
        crate::config::java_home().ok_or("JAVA_HOME is not set, and scip-java requires it")?;

    let runner = AspectRunner::new(
        root,
        config.bazel.as_deref().unwrap_or("bazel"),
        config.output_base.as_deref(),
    );
    let aspect = AspectConfig { targets: config.index.targets.clone(), scip_java, java_home };

    let started = std::time::Instant::now();
    runner.run(&aspect).map_err(|err| err.to_string())?;
    tracing::info!(elapsed = ?started.elapsed(), "indexed");
    Ok(())
}

/// Looks for SCIP shards under a workspace, returning the index it built.
///
/// The conventional home is `bazel-bin`, the convenience symlink Bazel writes at
/// the workspace root; shards land one per target beneath it.
///
/// A validated built-index snapshot is tried first. On a miss, reading every
/// shard is the only honest way to know the directory is usable; the resulting
/// index is handed to the server and cached asynchronously rather than rebuilt.
struct Discovered {
    dir: paths::Utf8PathBuf,
    index: SymbolIndex,
    shards: Vec<ShardMetadata>,
    provenance: Option<CacheKey>,
    cache: Option<CacheState>,
    cache_hit: bool,
    verified: bool,
}

#[derive(Clone)]
struct CacheState {
    root: PathBuf,
    dir: PathBuf,
    key: CacheKey,
    shards: Vec<ShardMetadata>,
}

struct RefreshResult {
    revision: u64,
    result: std::io::Result<RefreshWork>,
}

enum RefreshWork {
    Unchanged,
    Loaded(Box<SymbolIndex>, Vec<ShardMetadata>),
    Empty(Vec<ShardMetadata>),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum RefreshIntent {
    /// The current generation is complete; only normal cache reconciliation is needed.
    Idle,
    /// Validation failed or watcher events may have been lost. Retry safely.
    Retry,
    /// Git confirmed that sources moved. Wait for evidence of a new index build.
    AwaitingBuild,
}

fn provenance_is_current(provenance: Option<&CacheKey>, root: Option<&Path>, dir: &Path) -> bool {
    match (provenance, root) {
        (Some(key), Some(root)) => key.still_current(root, dir),
        _ => true,
    }
}

fn load_validated_generation(
    dir: &Path,
    provenance: Option<&CacheKey>,
    root: Option<&Path>,
) -> std::io::Result<symbol_index::ValidatedIndex> {
    if !provenance_is_current(provenance, root, dir) {
        return Err(std::io::Error::other("workspace moved before index reload"));
    }
    let loaded = SymbolIndex::load_validated(dir)?;
    if !provenance_is_current(provenance, root, dir) {
        return Err(std::io::Error::other("workspace moved during index reload"));
    }
    Ok(loaded)
}

fn load_validated_generation_from(
    dir: &Path,
    shards: Vec<ShardMetadata>,
    provenance: Option<&CacheKey>,
    root: Option<&Path>,
) -> std::io::Result<symbol_index::ValidatedIndex> {
    if !provenance_is_current(provenance, root, dir) {
        return Err(std::io::Error::other("workspace moved before index reload"));
    }
    let loaded = SymbolIndex::load_validated_shards(dir, shards)?;
    if !provenance_is_current(provenance, root, dir) {
        return Err(std::io::Error::other("workspace moved during index reload"));
    }
    Ok(loaded)
}

fn discover_index(root: &AbsPath, config: &Config) -> Option<Discovered> {
    // `bazel-bin` is a symlink into the output base; `symlink_metadata` would
    // see the link rather than the directory, so follow it deliberately.
    let dir = root.join("bazel-bin");
    if dir.as_utf8_path().is_dir() {
        let path = Path::new(dir.as_str());
        let key = CacheKey::new(Path::new(root.as_str()), path, config).ok();
        if let Some(key) = &key {
            match index_cache::load(Path::new(root.as_str()), key) {
                Ok(Some(hit)) if !hit.index.is_empty() => {
                    return Some(Discovered {
                        dir: dir.as_utf8_path().to_path_buf(),
                        index: hit.index,
                        shards: hit.shards.clone(),
                        provenance: Some(key.clone()),
                        cache: Some(CacheState {
                            root: PathBuf::from(root.as_str()),
                            dir: path.to_path_buf(),
                            key: key.clone(),
                            shards: hit.shards,
                        }),
                        cache_hit: true,
                        verified: false,
                    });
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(%err, "index cache unreadable; loading shards"),
            }
        }
        match load_validated_generation(path, key.as_ref(), Some(Path::new(root.as_str()))) {
            Ok(loaded) => {
                let index = loaded.index;
                if !index.is_empty() {
                    let cache = key.clone().map(|key| CacheState {
                        root: PathBuf::from(root.as_str()),
                        dir: path.to_path_buf(),
                        key,
                        shards: loaded.shards.clone(),
                    });
                    return Some(Discovered {
                        dir: dir.into_utf8_path_buf(),
                        index,
                        shards: loaded.shards,
                        provenance: key,
                        cache,
                        cache_hit: false,
                        verified: true,
                    });
                }
            }
            Err(err) => tracing::debug!(%dir, %err, "could not load a complete shard generation"),
        }
    }
    let dir = root.join(".jabar/index");
    if dir.as_utf8_path().is_dir() {
        let path = Path::new(dir.as_str());
        let provenance = CacheKey::new(Path::new(root.as_str()), path, config).ok();
        match load_validated_generation(path, provenance.as_ref(), Some(Path::new(root.as_str()))) {
            Ok(loaded) if !loaded.index.is_empty() => {
                return Some(Discovered {
                    dir: dir.into_utf8_path_buf(),
                    index: loaded.index,
                    shards: loaded.shards,
                    provenance,
                    cache: None,
                    cache_hit: false,
                    verified: true,
                });
            }
            Ok(_) => tracing::debug!(%dir, "no shards here"),
            Err(err) => tracing::debug!(%dir, %err, "could not read"),
        }
    }
    None
}

pub struct Server {
    sender: Sender<Message>,
    encoding: PositionEncoding,
    workspace_root: Option<AbsPathBuf>,
    /// Present once a workspace root is known. Queries against it come later.
    build: Option<BazelCli>,
    vfs: Vfs,
    documents: Documents,
    /// Files whose current source has not been incorporated into `index`.
    ///
    /// This survives save and close. Disk and editor agreement only means the
    /// save completed; it does not mean the SCIP aspect rebuilt the file.
    stale_documents: FxHashSet<VfsPath>,
    /// What the client asked for at startup.
    config: Config,
    /// Parses open files the index cannot see yet.
    ///
    /// Holds a parser, so it is kept rather than made per query.
    overlay: Overlay,
    /// Whether the client asked to be shown progress.
    pub supports_progress: bool,
    /// Watches the shards and git state; `None` until an index is loaded, since
    /// there is nothing to watch before that.
    watcher: Option<FileWatcher>,
    /// Where the shards were loaded from, so a change can reload them.
    index_dir: Option<paths::Utf8PathBuf>,
    /// The global symbol index, once one has been loaded.
    ///
    /// `None` means no index, which is a different answer from an empty one --
    /// see `handlers`. Loading is explicit for now: the aspect that produces
    /// shards has to run first, and jabar does not yet run it.
    index: Option<Arc<SymbolIndex>>,
    /// Manifest for the complete generation backing `index`, independent of
    /// whether that generation is eligible for the persistent cache.
    index_shards: Option<Vec<ShardMetadata>>,
    /// Workspace and output-tree identity captured for the loaded generation.
    provenance: Option<CacheKey>,
    cache: Option<CacheState>,
    cache_hit: bool,
    shards_verified: bool,
    cache_epoch: Arc<AtomicU64>,
    refresh_tx: Sender<RefreshResult>,
    refresh_rx: Receiver<RefreshResult>,
    refresh_revision: u64,
    refresh_running: bool,
    refresh_again: bool,
    refresh_intent: RefreshIntent,
    refresh_progress: Option<String>,
    telemetry: Telemetry,
    shutdown_requested: bool,
}

impl Server {
    pub fn new(
        sender: Sender<Message>,
        encoding: PositionEncoding,
        workspace_root: Option<AbsPathBuf>,
    ) -> Server {
        let build = workspace_root.clone().map(BazelCli::new);
        let (refresh_tx, refresh_rx) = crossbeam_channel::unbounded();
        Server {
            sender,
            encoding,
            workspace_root,
            build,
            vfs: Vfs::default(),
            documents: Documents::default(),
            stale_documents: FxHashSet::default(),
            index: None,
            index_shards: None,
            provenance: None,
            cache: None,
            cache_hit: false,
            shards_verified: false,
            cache_epoch: Arc::new(AtomicU64::new(0)),
            refresh_tx,
            refresh_rx,
            refresh_revision: 0,
            refresh_running: false,
            refresh_again: false,
            refresh_intent: RefreshIntent::Idle,
            refresh_progress: None,
            config: Config::default(),
            overlay: Overlay::new(),
            supports_progress: false,
            watcher: None,
            index_dir: None,
            telemetry: Telemetry::new(),
            shutdown_requested: false,
        }
    }

    /// Runs until the client exits or disconnects.
    ///
    /// The shutdown sequence is handled here rather than through
    /// [`Connection::handle_shutdown`], which treats anything but `exit` after a
    /// `shutdown` as a protocol error and ends the session. The spec is milder:
    /// the server keeps running and refuses further *requests* with
    /// `InvalidRequest` until `exit` arrives. A client that has a request in
    /// flight when the user quits should get an error, not a dead socket.
    fn run(mut self, connection: &Connection) -> anyhow::Result<()> {
        let refresh_tick = crossbeam_channel::tick(Duration::from_secs(300));
        loop {
            // The watcher channel is swapped in as the index is loaded, so it is
            // re-read each turn rather than captured once. `never()` parks the
            // arm until there is something to watch.
            let watch_rx = match &self.watcher {
                Some(watcher) => watcher.receiver().clone(),
                None => crossbeam_channel::never(),
            };
            let message = crossbeam_channel::select! {
                recv(connection.receiver) -> message => match message {
                    Ok(message) => message,
                    // The client hung up. Common when an editor is killed, and
                    // not worth failing over.
                    Err(_) => {
                        tracing::info!("client disconnected");
                        return Ok(());
                    }
                },
                recv(watch_rx) -> change => {
                    if let Ok(change) = change {
                        self.on_file_change(change);
                    }
                    continue;
                },
                recv(self.refresh_rx) -> result => {
                    if let Ok(result) = result {
                        self.on_refresh_result(result);
                    }
                    continue;
                },
                recv(refresh_tick) -> _ => {
                    self.on_refresh_tick();
                    continue;
                },
            };
            match message {
                Message::Request(request) => {
                    if request.method == lsp_types::request::Shutdown::METHOD {
                        tracing::info!("client requested shutdown");
                        self.shutdown_requested = true;
                        self.send(Response::new_ok(request.id, ()).into());
                        continue;
                    }
                    self.on_request(request);
                }
                Message::Notification(notification) => {
                    if notification.method == lsp_types::notification::Exit::METHOD {
                        // Exiting without a prior `shutdown` is a client bug,
                        // and one worth reporting: it usually means the client
                        // crashed rather than closed down.
                        anyhow::ensure!(
                            self.shutdown_requested,
                            "client sent `exit` without `shutdown`"
                        );
                        tracing::info!("client exited");
                        return Ok(());
                    }
                    self.on_notification(notification);
                }
                // Nothing sends client-bound requests yet, so any response is
                // one we never asked for.
                Message::Response(response) => {
                    tracing::warn!(id = ?response.id, "unsolicited response");
                }
            }
        }
    }

    fn on_refresh_tick(&mut self) {
        // A branch switch makes existing shards suspect until a build changes
        // them. A timer is not evidence of a build and must not resurrect the
        // old branch's index.
        if self.refresh_intent == RefreshIntent::AwaitingBuild {
            return;
        }
        if self.cache.is_some() || self.refresh_intent == RefreshIntent::Retry {
            self.schedule_index_refresh(false, self.index.is_none());
        }
    }

    /// Reacts to something changing on disk without loading shards on the loop.
    fn on_file_change(&mut self, change: Change) {
        match change {
            Change::Index => {
                tracing::info!("shards changed on disk; scheduling an index reload");
                let force =
                    self.refresh_intent == RefreshIntent::AwaitingBuild || self.index.is_none();
                self.refresh_intent = RefreshIntent::Retry;
                self.schedule_index_refresh(true, force);
            }
            Change::Workspace => {
                // A branch switch invalidates the index without necessarily
                // rewriting any shard: the shards on disk now describe the old
                // tree. Nothing can be reloaded that would be right, so the
                // honest move is to drop the index and say so.
                tracing::info!("the workspace moved; dropping the index as stale");
                self.index = None;
                self.cache_hit = false;
                self.shards_verified = false;
                self.refresh_intent = RefreshIntent::AwaitingBuild;
                self.refresh_again = false;
                self.refresh_revision = self.refresh_revision.wrapping_add(1);
                self.cache_epoch.fetch_add(1, Ordering::SeqCst);
                if let Some(cache) = &mut self.cache
                    && let Ok(key) = CacheKey::new(&cache.root, &cache.dir, &self.config)
                {
                    cache.key = key.clone();
                    self.provenance = Some(key);
                } else if let (Some(root), Some(dir)) = (&self.workspace_root, &self.index_dir) {
                    self.provenance = CacheKey::new(
                        Path::new(root.as_str()),
                        Path::new(dir.as_str()),
                        &self.config,
                    )
                    .ok();
                }
                self.notify_index_stale();
            }
            Change::WatcherError => {
                tracing::warn!("watcher may have dropped events; validating the current index");
                if self.refresh_intent == RefreshIntent::AwaitingBuild {
                    return;
                }
                self.shards_verified = false;
                self.refresh_intent = RefreshIntent::Retry;
                self.schedule_index_refresh(true, self.index.is_none());
            }
        }
    }

    fn schedule_index_refresh(&mut self, changed: bool, force: bool) {
        let Some(dir) = self.index_dir.as_ref().map(|dir| PathBuf::from(dir.as_str())) else {
            return;
        };
        let previous = self.index_shards.clone();
        let provenance = self.provenance.clone();
        let root = self.workspace_root.as_ref().map(|root| PathBuf::from(root.as_str()));
        self.schedule_refresh(changed, move || {
            if !provenance_is_current(provenance.as_ref(), root.as_deref(), &dir) {
                return Err(std::io::Error::other("workspace moved before index reload"));
            }
            let shards = SymbolIndex::scan_shards(&dir)?;
            if !force && previous.as_ref().is_some_and(|previous| *previous == shards) {
                if !provenance_is_current(provenance.as_ref(), root.as_deref(), &dir) {
                    return Err(std::io::Error::other("workspace moved during index validation"));
                }
                return Ok(RefreshWork::Unchanged);
            }
            let loaded =
                load_validated_generation_from(&dir, shards, provenance.as_ref(), root.as_deref())?;
            if loaded.shards.is_empty() {
                return Ok(RefreshWork::Empty(loaded.shards));
            }
            let index = loaded.index;
            if index.is_empty() {
                return Err(std::io::Error::other("no usable SCIP shards after reload"));
            }
            Ok(RefreshWork::Loaded(Box::new(index), loaded.shards))
        });
    }

    fn schedule_refresh(
        &mut self,
        changed: bool,
        work: impl FnOnce() -> std::io::Result<RefreshWork> + Send + 'static,
    ) {
        if self.refresh_running {
            if changed {
                self.refresh_revision = self.refresh_revision.wrapping_add(1);
                self.refresh_again = true;
            }
            return;
        }
        self.refresh_revision = self.refresh_revision.wrapping_add(1);
        self.refresh_running = true;
        self.refresh_progress = self.begin_progress("jabar", "reloading the symbol index");
        let tx = self.refresh_tx.clone();
        let revision = self.refresh_revision;
        std::thread::spawn(move || {
            let _ = tx.send(RefreshResult { revision, result: work() });
        });
    }

    fn on_refresh_result(&mut self, result: RefreshResult) {
        self.refresh_running = false;
        let progress = self.refresh_progress.take();
        self.end_progress(progress);
        if result.revision == self.refresh_revision {
            match result.result {
                Ok(RefreshWork::Unchanged) if self.index.is_some() => {
                    self.shards_verified = true;
                    self.refresh_intent = RefreshIntent::Idle;
                }
                Ok(RefreshWork::Unchanged) => {
                    self.shards_verified = false;
                    self.refresh_intent = RefreshIntent::Retry;
                }
                Ok(RefreshWork::Empty(shards)) => {
                    tracing::info!("all indexed shards were removed; clearing the index");
                    self.index = None;
                    self.index_shards = Some(shards.clone());
                    if let Some(cache) = &mut self.cache {
                        cache.shards = shards;
                        index_cache::invalidate(&cache.root);
                    }
                    self.cache_hit = false;
                    self.shards_verified = true;
                    self.refresh_intent = RefreshIntent::Idle;
                    self.cache_epoch.fetch_add(1, Ordering::SeqCst);
                    self.notify_index_stale();
                }
                Ok(RefreshWork::Loaded(index, shards)) => {
                    let was_unavailable = self.index.is_none();
                    let definitions = index.definition_count();
                    tracing::info!(shards = index.shard_count(), definitions, "index reloaded");
                    self.index = Some(Arc::from(index));
                    self.refresh_document_freshness();
                    self.index_shards = Some(shards.clone());
                    self.shards_verified = true;
                    self.refresh_intent = RefreshIntent::Idle;
                    if let Some(cache) = &mut self.cache {
                        cache.shards = shards;
                        self.cache_hit = false;
                        self.write_cache_async();
                    }
                    if was_unavailable {
                        self.register_workspace_symbol();
                    }
                }
                Err(err) => {
                    self.shards_verified = false;
                    if self.refresh_intent != RefreshIntent::AwaitingBuild {
                        self.refresh_intent = RefreshIntent::Retry;
                    }
                    tracing::warn!(%err, "could not reconcile shards; keeping the old index");
                }
            }
        }
        if self.refresh_again {
            self.refresh_again = false;
            if self.refresh_intent != RefreshIntent::AwaitingBuild {
                self.schedule_index_refresh(false, self.index.is_none());
            }
        }
    }

    /// Tells the client that something slow has started, if it can show that.
    ///
    /// Returns the token to pass to [`Server::end_progress`], or `None` when
    /// the client does not support progress — in which case ending is a no-op.
    fn begin_progress(&self, title: &str, message: &str) -> Option<String> {
        if !self.supports_progress {
            return None;
        }
        let token = format!("jabar-{}", std::process::id());
        // `create` first: a `$/progress` for a token the client has not been
        // told about is discarded by most clients.
        let create = lsp_types::WorkDoneProgressCreateParams {
            token: lsp_types::NumberOrString::String(token.clone()),
        };
        if let Ok(params) = serde_json::to_value(create) {
            self.send(
                lsp_server::Request::new(
                    lsp_server::RequestId::from(format!("{token}-create")),
                    lsp_types::request::WorkDoneProgressCreate::METHOD.to_owned(),
                    params,
                )
                .into(),
            );
        }
        self.send_progress(
            &token,
            lsp_types::WorkDoneProgress::Begin(lsp_types::WorkDoneProgressBegin {
                title: title.to_owned(),
                cancellable: Some(false),
                message: Some(message.to_owned()),
                percentage: None,
            }),
        );
        Some(token)
    }

    fn end_progress(&self, token: Option<String>) {
        let Some(token) = token else { return };
        self.send_progress(
            &token,
            lsp_types::WorkDoneProgress::End(lsp_types::WorkDoneProgressEnd { message: None }),
        );
    }

    fn send_progress(&self, token: &str, value: lsp_types::WorkDoneProgress) {
        let params = lsp_types::ProgressParams {
            token: lsp_types::NumberOrString::String(token.to_owned()),
            value: lsp_types::ProgressParamsValue::WorkDone(value),
        };
        match serde_json::to_value(params) {
            Ok(params) => self.send(
                lsp_server::Notification::new(
                    lsp_types::notification::Progress::METHOD.to_owned(),
                    params,
                )
                .into(),
            ),
            Err(err) => tracing::debug!(%err, "could not build a progress notification"),
        }
    }

    /// Tells the client the index is gone, so it can stop trusting past answers.
    fn notify_index_stale(&self) {
        let params = lsp_types::ShowMessageParams {
            typ: lsp_types::MessageType::WARNING,
            message: "jabar: the workspace moved and the symbol index is now stale. \
                      Re-run the SCIP aspect and call `jabar/loadIndex`."
                .to_owned(),
        };
        match serde_json::to_value(params) {
            Ok(params) => self.send(
                lsp_server::Notification::new(
                    lsp_types::notification::ShowMessage::METHOD.to_owned(),
                    params,
                )
                .into(),
            ),
            Err(err) => tracing::warn!(%err, "could not build the staleness notification"),
        }
    }

    fn on_request(&mut self, request: Request) {
        let id = request.id.clone();

        // A handler that unwinds must not take the session with it, and the
        // client is still owed a response.
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| self.dispatch(request)));

        let response = match outcome {
            Ok(Ok(value)) => Response::new_ok(id, value),
            Ok(Err(err)) => {
                tracing::warn!(%err, "request failed");
                Response::new_err(id, err.code as i32, err.message)
            }
            Err(_panic) => {
                tracing::error!(?id, "handler panicked");
                Response::new_err(
                    id,
                    ErrorCode::InternalError as i32,
                    "internal error; the server has logged it".to_owned(),
                )
            }
        };
        self.send(response.into());
    }

    fn dispatch(&mut self, request: Request) -> Result<serde_json::Value, RequestError> {
        // Requests arriving after `shutdown` must be refused, per the spec.
        if self.shutdown_requested {
            return Err(RequestError::new(
                ErrorCode::InvalidRequest,
                "server is shutting down".to_owned(),
            ));
        }

        match request.method.as_str() {
            STATUS_REQUEST => Ok(serde_json::to_value(self.status())?),
            LOAD_INDEX_REQUEST => self.load_index(request.params),
            lsp_types::request::WorkspaceSymbolRequest::METHOD => {
                self.workspace_symbol(request.params)
            }
            lsp_types::request::GotoDefinition::METHOD => self.goto_definition(request.params),
            lsp_types::request::References::METHOD => {
                self.find_references(request.params).map(|r| r.locations)
            }
            REFERENCES_REQUEST => self.find_references(request.params).map(|r| r.full),
            lsp_types::request::HoverRequest::METHOD => self.hover(request.params),
            lsp_types::request::GotoImplementation::METHOD => {
                self.goto_implementation(request.params)
            }
            lsp_types::request::DocumentSymbolRequest::METHOD => {
                self.document_symbols(request.params)
            }
            lsp_types::request::CallHierarchyPrepare::METHOD => {
                self.prepare_call_hierarchy(request.params)
            }
            lsp_types::request::CallHierarchyIncomingCalls::METHOD => {
                self.call_hierarchy(request.params, CallDirection::Incoming)
            }
            lsp_types::request::CallHierarchyOutgoingCalls::METHOD => {
                self.call_hierarchy(request.params, CallDirection::Outgoing)
            }
            // Refusing loudly matters: silently returning null would look to a
            // client like a successful empty answer.
            unknown => Err(RequestError::new(
                ErrorCode::MethodNotFound,
                format!("`{unknown}` is not implemented"),
            )),
        }
    }

    pub fn apply_config(&mut self, config: Config) {
        self.build = self.workspace_root.clone().map(|root| {
            BazelCli::new(root)
                .with_program(config.bazel.clone().unwrap_or_else(|| "bazel".to_owned()))
                .with_output_base(config.output_base.clone())
        });
        self.config = config;
    }

    /// Takes on an index discovered at startup.
    ///
    /// Takes the index by value rather than re-reading it: discovery already
    /// paid for the read, and doing it twice was ~350ms of startup latency on
    /// Gerrit for nothing.
    pub fn adopt_index(&mut self, dir: paths::Utf8PathBuf, index: SymbolIndex) {
        self.refresh_revision = self.refresh_revision.wrapping_add(1);
        self.cache_epoch.fetch_add(1, Ordering::SeqCst);
        self.cache = None;
        self.cache_hit = false;
        self.shards_verified = false;
        self.index_shards = None;
        self.refresh_intent = RefreshIntent::Retry;
        self.provenance = self.workspace_root.as_ref().and_then(|root| {
            CacheKey::new(Path::new(root.as_str()), Path::new(dir.as_str()), &self.config).ok()
        });
        self.index = Some(Arc::new(index));
        self.refresh_document_freshness();
        self.start_watching(Some(dir.as_str()));
        self.index_dir = Some(dir);
    }

    fn adopt_discovered(&mut self, discovered: Discovered) {
        self.refresh_revision = self.refresh_revision.wrapping_add(1);
        self.cache = discovered.cache;
        self.cache_hit = discovered.cache_hit;
        self.shards_verified = discovered.verified;
        self.index_shards = Some(discovered.shards);
        self.provenance = discovered.provenance;
        self.refresh_intent =
            if discovered.verified { RefreshIntent::Idle } else { RefreshIntent::Retry };
        self.index = Some(Arc::new(discovered.index));
        self.refresh_document_freshness();
        // Watching millions of bazel-bin entries recursively can itself stall
        // startup. Cached indexes reconcile in a worker instead.
        let watch_dir = self.cache.is_none().then_some(discovered.dir.as_str());
        self.start_watching(watch_dir);
        self.index_dir = Some(discovered.dir);
        if self.cache_hit {
            self.schedule_index_refresh(false, false);
        } else if self.cache.is_some() {
            self.write_cache_async();
        }
    }

    fn write_cache_async(&self) {
        let (Some(cache), Some(index)) = (self.cache.clone(), self.index.clone()) else {
            return;
        };
        let epoch = Arc::clone(&self.cache_epoch);
        let ticket = epoch.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        std::thread::spawn(move || {
            match index_cache::store(
                &cache.root,
                &cache.dir,
                &cache.key,
                &cache.shards,
                &index,
                || epoch.load(Ordering::SeqCst) == ticket,
            ) {
                Ok(()) => tracing::info!("built index cache published"),
                Err(err) => tracing::warn!(%err, "could not publish index cache"),
            }
        });
    }

    /// Loads a directory of SCIP shards into the global index.
    ///
    /// Explicit rather than automatic because the index is a build output: the
    /// aspect has to have run. Wiring jabar to run it is M4 (see F17).
    fn load_index(&mut self, params: serde_json::Value) -> Result<serde_json::Value, RequestError> {
        #[derive(serde::Deserialize)]
        struct Params {
            path: String,
        }
        let params: Params = serde_json::from_value(params).map_err(|err| {
            RequestError::new(ErrorCode::InvalidParams, format!("expected {{path}}: {err}"))
        })?;

        let mut guard = self.telemetry.start(telemetry::Op::IndexBuild);
        let path = std::path::Path::new(&params.path);
        let root = self.workspace_root.as_ref().map(|root| PathBuf::from(root.as_str()));
        let provenance =
            root.as_deref().and_then(|root| CacheKey::new(root, path, &self.config).ok());
        let loaded = load_validated_generation(path, provenance.as_ref(), root.as_deref())
            .map_err(|err| {
                guard_failed(&mut guard);
                RequestError::new(
                    ErrorCode::InvalidParams,
                    format!("could not load a complete index from `{}`: {err}", params.path),
                )
            })?;
        let index = loaded.index;
        let manifest = loaded.shards;

        let (shards, definitions) = (index.shard_count(), index.definition_count());
        guard.finish(if definitions == 0 {
            telemetry::Outcome::Empty { reason: telemetry::EmptyReason::NoMatch }
        } else {
            telemetry::Outcome::answered(definitions)
        });

        tracing::info!(shards, definitions, path = %params.path, "index loaded");
        self.refresh_revision = self.refresh_revision.wrapping_add(1);
        self.index = Some(Arc::new(index));
        self.refresh_document_freshness();
        self.cache = None;
        self.cache_epoch.fetch_add(1, Ordering::SeqCst);
        self.cache_hit = false;
        self.shards_verified = true;
        self.index_shards = Some(manifest);
        self.provenance = provenance;
        self.refresh_intent = RefreshIntent::Idle;
        self.index_dir = Some(paths::Utf8PathBuf::from(params.path.clone()));
        self.start_watching(Some(&params.path));
        // The capability was not advertised at initialize, because there was
        // nothing behind it. Tell the client it exists now.
        self.register_workspace_symbol();

        Ok(serde_json::json!({ "shards": shards, "definitions": definitions }))
    }

    fn workspace_symbol(
        &mut self,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RequestError> {
        let params: lsp_types::WorkspaceSymbolParams = serde_json::from_value(params)
            .map_err(|err| RequestError::new(ErrorCode::InvalidParams, err.to_string()))?;

        let mut guard = self.telemetry.start(telemetry::Op::WorkspaceSymbol);
        guard.at_revision(self.vfs.revision().as_u64());

        // No index is not the same answer as no matches. Returning `[]` here
        // would tell the client the symbol does not exist, which it cannot
        // distinguish and would act on.
        if self.index.is_none() || self.workspace_root.is_none() {
            guard.finish(handlers::index_unavailable_outcome());
            return Err(RequestError::new(
                ErrorCode::ServerNotInitialized,
                "no symbol index is loaded; run the SCIP aspect and call `jabar/loadIndex`"
                    .to_owned(),
            ));
        }
        let root = self.workspace_root.clone().expect("just checked");

        // Parse every open file and every saved file known to be newer than the
        // index, so a symbol written seconds ago is findable. This stays bounded
        // by files edited during the session rather than the size of the repo.
        let mut shadowed_set = FxHashSet::default();
        let mut current_sources: Vec<(String, String)> = Vec::new();
        for (path, document) in self.documents.iter() {
            let Some(relative) = path
                .as_real()
                .and_then(|abs| abs.strip_prefix(&root))
                .map(|path| path.as_str().to_owned())
            else {
                continue;
            };
            shadowed_set.insert(relative.clone());
            current_sources.push((relative, document.text.clone()));
        }
        // Saved and closed files can still be newer than the index. Parse their
        // disk contents rather than reviving stale indexed declarations.
        for path in &self.stale_documents {
            let Some((relative, abs)) = path.as_real().and_then(|abs| {
                let relative = abs.strip_prefix(&root)?.as_str().to_owned();
                Some((relative, abs))
            }) else {
                continue;
            };
            if shadowed_set.insert(relative.clone())
                && let Ok(text) = std::fs::read_to_string(abs.as_str())
            {
                current_sources.push((relative, text));
            }
        }
        let live: Vec<symbol_index::Definition> = current_sources
            .iter()
            .flat_map(|(path, text)| self.overlay.parse(path, text))
            .collect();
        let shadowed: Vec<String> = shadowed_set.into_iter().collect();

        let index = self.index.as_ref().expect("resolve_query established there is one");
        let read = file_reader(&self.documents, &root);
        let results = handlers::workspace_symbol_with(
            index,
            &live,
            &shadowed,
            &params.query,
            &root,
            self.encoding,
            read,
        );

        guard.finish(results.outcome());
        Ok(serde_json::to_value(results.symbols)?)
    }

    fn goto_definition(
        &mut self,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RequestError> {
        let params: lsp_types::GotoDefinitionParams = serde_json::from_value(params)
            .map_err(|err| RequestError::new(ErrorCode::InvalidParams, err.to_string()))?;
        let doc = params.text_document_position_params;

        let mut guard = self.telemetry.start(telemetry::Op::GoToDefinition);
        guard.at_revision(self.vfs.revision().as_u64());

        let Some((index, root, relative)) = self.resolve_query(&doc.text_document.uri) else {
            let err = self.refuse(&mut guard, &doc.text_document.uri);
            return Err(err);
        };
        self.require_current_document(&mut guard, &relative, root)?;
        let position =
            crate::line_index::LinePosition::new(doc.position.line, doc.position.character);
        let read = file_reader(&self.documents, root);

        match handlers::goto_definition(index, &relative, position, root, self.encoding, &read) {
            Some(found) => {
                tracing::debug!(symbol = %found.symbol, "resolved definition");
                guard.finish(telemetry::Outcome::answered(1));
                Ok(serde_json::to_value(lsp_types::GotoDefinitionResponse::Scalar(found.location))?)
            }
            None => {
                // Nothing at that position, or a symbol this index does not
                // define -- a JDK or third-party type in a jar no shard covers.
                // Dirty documents were refused before lookup, so this is a
                // truthful absence in the generation being queried.
                guard.finish(telemetry::Outcome::Empty { reason: telemetry::EmptyReason::NoMatch });
                Ok(serde_json::Value::Null)
            }
        }
    }

    fn find_references(
        &mut self,
        params: serde_json::Value,
    ) -> Result<ReferenceReply, RequestError> {
        let params: lsp_types::ReferenceParams = serde_json::from_value(params)
            .map_err(|err| RequestError::new(ErrorCode::InvalidParams, err.to_string()))?;
        let doc = params.text_document_position;
        let include_declaration = params.context.include_declaration;

        let mut guard = self.telemetry.start(telemetry::Op::FindReferences);
        guard.at_revision(self.vfs.revision().as_u64());

        let Some((index, root, relative)) = self.resolve_query(&doc.text_document.uri) else {
            let err = self.refuse(&mut guard, &doc.text_document.uri);
            return Err(err);
        };
        self.require_current_document(&mut guard, &relative, root)?;
        let position =
            crate::line_index::LinePosition::new(doc.position.line, doc.position.character);
        let read = file_reader(&self.documents, root);

        let reply = match handlers::find_references(
            index,
            &relative,
            position,
            include_declaration,
            root,
            self.encoding,
            &read,
        ) {
            Some(results) => {
                if results.outcome().is_truncated() {
                    tracing::info!(
                        symbol = %results.symbol,
                        returned = results.locations.len(),
                        total = results.total,
                        "truncated references"
                    );
                }
                guard.finish(results.outcome());
                ReferenceReply::new(&results.symbol, results.locations, results.total)?
            }
            None => {
                guard.finish(telemetry::Outcome::Empty { reason: telemetry::EmptyReason::NoMatch });
                ReferenceReply::new("", Vec::new(), 0)?
            }
        };
        Ok(reply)
    }

    fn hover(&mut self, params: serde_json::Value) -> Result<serde_json::Value, RequestError> {
        let params: lsp_types::HoverParams = serde_json::from_value(params)
            .map_err(|err| RequestError::new(ErrorCode::InvalidParams, err.to_string()))?;
        let doc = params.text_document_position_params;

        let mut guard = self.telemetry.start(telemetry::Op::Hover);
        guard.at_revision(self.vfs.revision().as_u64());

        let Some((index, root, relative)) = self.resolve_query(&doc.text_document.uri) else {
            let err = self.refuse(&mut guard, &doc.text_document.uri);
            return Err(err);
        };
        self.require_current_document(&mut guard, &relative, root)?;
        let position =
            crate::line_index::LinePosition::new(doc.position.line, doc.position.character);
        let read = file_reader(&self.documents, root);

        match handlers::hover(index, &relative, position, self.encoding, &read) {
            Some(hover) => {
                guard.finish(telemetry::Outcome::answered(1));
                Ok(serde_json::to_value(hover)?)
            }
            None => {
                guard.finish(telemetry::Outcome::Empty { reason: telemetry::EmptyReason::NoMatch });
                Ok(serde_json::Value::Null)
            }
        }
    }

    fn goto_implementation(
        &mut self,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RequestError> {
        let params: lsp_types::request::GotoImplementationParams =
            serde_json::from_value(params)
                .map_err(|err| RequestError::new(ErrorCode::InvalidParams, err.to_string()))?;
        let doc = params.text_document_position_params;

        let mut guard = self.telemetry.start(telemetry::Op::GoToImplementation);
        guard.at_revision(self.vfs.revision().as_u64());

        let Some((index, root, relative)) = self.resolve_query(&doc.text_document.uri) else {
            let err = self.refuse(&mut guard, &doc.text_document.uri);
            return Err(err);
        };
        self.require_current_document(&mut guard, &relative, root)?;
        let position =
            crate::line_index::LinePosition::new(doc.position.line, doc.position.character);
        let read = file_reader(&self.documents, root);

        match handlers::goto_implementation(index, &relative, position, root, self.encoding, &read)
        {
            Some(locations) if !locations.is_empty() => {
                guard.finish(telemetry::Outcome::answered(locations.len()));
                Ok(serde_json::to_value(lsp_types::GotoDefinitionResponse::Array(locations))?)
            }
            // A concrete class with no subtypes genuinely has no
            // implementations, which is a truthful empty rather than a failure.
            _ => {
                guard.finish(telemetry::Outcome::Empty { reason: telemetry::EmptyReason::NoMatch });
                Ok(serde_json::to_value(Vec::<lsp_types::Location>::new())?)
            }
        }
    }

    fn document_symbols(
        &mut self,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RequestError> {
        let params: lsp_types::DocumentSymbolParams = serde_json::from_value(params)
            .map_err(|err| RequestError::new(ErrorCode::InvalidParams, err.to_string()))?;

        let mut guard = self.telemetry.start(telemetry::Op::DocumentSymbol);
        guard.at_revision(self.vfs.revision().as_u64());

        // Resolve to owned values first: parsing needs `&mut self`, and a
        // borrow of the index would still be alive otherwise.
        let (root, relative) = {
            let Some((_, root, relative)) = self.resolve_query(&params.text_document.uri) else {
                let err = self.refuse(&mut guard, &params.text_document.uri);
                return Err(err);
            };
            (root.clone(), relative)
        };

        // The client's buffer is the truth for an open file, and the index
        // describes the tree as of the last build. Parsing what the client
        // holds is both more current and the only way to see a file that was
        // never built.
        let path = VfsPath::Real(root.join(&relative));
        let live_text = self.documents.get(&path).map(|doc| doc.text.clone());
        let live = if let Some(text) = live_text {
            Some(self.overlay.parse(&relative, &text))
        } else if self.stale_documents.contains(&path) {
            // A saved, closed file remains newer than the index. Failure to
            // read it still shadows old declarations rather than reviving them.
            Some(
                std::fs::read_to_string(root.join(&relative).as_str())
                    .map(|text| self.overlay.parse(&relative, &text))
                    .unwrap_or_default(),
            )
        } else {
            None
        };

        let index = self.index.as_ref().expect("resolve_query established there is one");
        let read = file_reader(&self.documents, &root);
        let symbols = match &live {
            Some(defs) => {
                let borrowed: Vec<&symbol_index::Definition> = defs.iter().collect();
                handlers::document_symbols_from(&borrowed, &relative, self.encoding, &read)
            }
            // Only a closed file may fall back to the built index. An empty or
            // temporarily unparseable open buffer is still authoritative.
            None => handlers::document_symbols(index, &relative, self.encoding, &read),
        };

        if symbols.is_empty() {
            // A file the index does not cover -- not yet built, or not Java.
            // That is not "this file declares nothing", so say which it is.
            guard.finish(telemetry::Outcome::Empty {
                reason: telemetry::EmptyReason::FileNotIndexed,
            });
        } else {
            guard.finish(telemetry::Outcome::answered(symbols.len()));
        }
        Ok(serde_json::to_value(lsp_types::DocumentSymbolResponse::Nested(symbols))?)
    }

    /// Refuses a positional query when its coordinates refer to newer source.
    fn require_current_document(
        &self,
        guard: &mut telemetry::InFlight<'_>,
        relative: &str,
        root: &AbsPathBuf,
    ) -> Result<(), RequestError> {
        let path = VfsPath::Real(root.join(relative));
        if !self.stale_documents.contains(&path) {
            return Ok(());
        }

        guard.mark_stale(true);
        guard.mark_failed(telemetry::Failure::IndexUnavailable);
        Err(RequestError::new(
            ErrorCode::ContentModified,
            format!(
                "`{relative}` changed after the symbol index was built; rebuild the index before running a positional query"
            ),
        ))
    }

    /// Re-establishes per-file correspondence after installing a generation.
    ///
    /// Closed files now match the build by construction. Open buffers remain
    /// stale when they differ from disk, since the build could not have seen
    /// those unsaved bytes.
    fn refresh_document_freshness(&mut self) {
        self.stale_documents = self
            .documents
            .iter()
            .filter_map(|(path, document)| {
                let abs = path.as_real()?;
                let matches_disk =
                    std::fs::read_to_string(abs.as_str()).is_ok_and(|disk| disk == document.text);
                (!matches_disk).then(|| path.clone())
            })
            .collect();
    }

    fn prepare_call_hierarchy(
        &mut self,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RequestError> {
        let params: lsp_types::CallHierarchyPrepareParams = serde_json::from_value(params)
            .map_err(|err| RequestError::new(ErrorCode::InvalidParams, err.to_string()))?;
        let doc = params.text_document_position_params;

        let mut guard = self.telemetry.start(telemetry::Op::PrepareCallHierarchy);
        guard.at_revision(self.vfs.revision().as_u64());

        let Some((index, root, relative)) = self.resolve_query(&doc.text_document.uri) else {
            let err = self.refuse(&mut guard, &doc.text_document.uri);
            return Err(err);
        };
        self.require_current_document(&mut guard, &relative, root)?;
        let position =
            crate::line_index::LinePosition::new(doc.position.line, doc.position.character);
        let read = file_reader(&self.documents, root);

        match handlers::prepare_call_hierarchy(
            index,
            &relative,
            position,
            root,
            self.encoding,
            &read,
        ) {
            Some(item) => {
                guard.finish(telemetry::Outcome::answered(1));
                Ok(serde_json::to_value(vec![item])?)
            }
            None => {
                // Not on a callable, or on one the index does not define. LSP
                // wants null rather than an empty array to mean "no hierarchy
                // starts here".
                guard.finish(telemetry::Outcome::Empty { reason: telemetry::EmptyReason::NoMatch });
                Ok(serde_json::Value::Null)
            }
        }
    }

    fn call_hierarchy(
        &mut self,
        params: serde_json::Value,
        direction: CallDirection,
    ) -> Result<serde_json::Value, RequestError> {
        // Both directions carry the same item; only the method name differs.
        #[derive(serde::Deserialize)]
        struct Params {
            item: lsp_types::CallHierarchyItem,
        }
        let params: Params = serde_json::from_value(params)
            .map_err(|err| RequestError::new(ErrorCode::InvalidParams, err.to_string()))?;

        let op = match direction {
            CallDirection::Incoming => telemetry::Op::IncomingCalls,
            CallDirection::Outgoing => telemetry::Op::OutgoingCalls,
        };
        let mut guard = self.telemetry.start(op);
        guard.at_revision(self.vfs.revision().as_u64());

        let Some((index, root, relative)) = self.resolve_query(&params.item.uri) else {
            let err = self.refuse(&mut guard, &params.item.uri);
            return Err(err);
        };
        self.require_current_document(&mut guard, &relative, root)?;
        // The symbol was stashed at prepare time. A client that fabricates an
        // item, or replays one from a previous session, will not have it.
        let Some(symbol) = handlers::call_item_symbol(&params.item) else {
            guard.mark_failed(telemetry::Failure::BadRequest);
            return Err(RequestError::new(
                ErrorCode::InvalidParams,
                "the call hierarchy item did not come from `prepareCallHierarchy`".to_owned(),
            ));
        };

        let read = file_reader(&self.documents, root);
        let value = match direction {
            CallDirection::Incoming => {
                let calls = handlers::incoming_calls(index, &symbol, root, self.encoding, &read);
                guard.finish(count_outcome(calls.len()));
                serde_json::to_value(calls)?
            }
            CallDirection::Outgoing => {
                let calls = handlers::outgoing_calls(index, &symbol, root, self.encoding, &read);
                guard.finish(count_outcome(calls.len()));
                serde_json::to_value(calls)?
            }
        };
        Ok(value)
    }

    /// The index, workspace root, and workspace-relative path for a query.
    ///
    /// `None` when there is no index, or the URI is not a real path under the
    /// workspace. The caller turns that into a refusal rather than an empty
    /// answer.
    fn resolve_query(&self, uri: &lsp_types::Url) -> Option<(&SymbolIndex, &AbsPathBuf, String)> {
        let index = self.index.as_ref()?;
        let root = self.workspace_root.as_ref()?;
        let path = uri::vfs_path(uri).ok()?;
        let abs = path.as_real()?.clone();
        let relative = abs.strip_prefix(root)?.as_str().to_owned();
        Some((index, root, relative))
    }

    /// Records the refusal and builds the error the client sees.
    fn refuse(&self, guard: &mut telemetry::InFlight<'_>, uri: &lsp_types::Url) -> RequestError {
        if self.index.is_none() {
            guard.mark_failed(telemetry::Failure::IndexUnavailable);
            RequestError::new(
                ErrorCode::ServerNotInitialized,
                "no symbol index is loaded; run the SCIP aspect and call `jabar/loadIndex`"
                    .to_owned(),
            )
        } else {
            guard.mark_failed(telemetry::Failure::BadRequest);
            RequestError::new(
                ErrorCode::InvalidParams,
                format!("`{uri}` is not a file inside the workspace"),
            )
        }
    }

    /// Begins watching the shards and the workspace's git state.
    ///
    /// Failing to watch is not failing to serve: the index is loaded and every
    /// query still works, it just will not notice a rebuild. Worth a warning,
    /// not an error.
    fn start_watching(&mut self, index_dir: Option<&str>) {
        let index_dir = index_dir.map(paths::Utf8Path::new).and_then(paths::AbsPath::try_new);
        match FileWatcher::spawn(index_dir, self.workspace_root.as_deref()) {
            Ok(watcher) => {
                tracing::debug!(?index_dir, "watching for index changes");
                self.watcher = Some(watcher);
            }
            Err(err) => tracing::warn!(%err, "not watching for changes; reloads must be manual"),
        }
    }

    /// Registers `workspace/symbol` dynamically, now that it can be served.
    fn register_workspace_symbol(&mut self) {
        let registrations = [
            lsp_types::request::WorkspaceSymbolRequest::METHOD,
            lsp_types::request::GotoDefinition::METHOD,
            lsp_types::request::References::METHOD,
            lsp_types::request::HoverRequest::METHOD,
            lsp_types::request::GotoImplementation::METHOD,
            lsp_types::request::DocumentSymbolRequest::METHOD,
            lsp_types::request::CallHierarchyPrepare::METHOD,
        ]
        .into_iter()
        .map(|method| lsp_types::Registration {
            id: format!("jabar-{method}"),
            method: method.to_owned(),
            register_options: None,
        })
        .collect();
        let params = lsp_types::RegistrationParams { registrations };
        match serde_json::to_value(params) {
            Ok(params) => self.send(
                lsp_server::Request::new(
                    lsp_server::RequestId::from("jabar-register-workspace-symbol".to_owned()),
                    lsp_types::request::RegisterCapability::METHOD.to_owned(),
                    params,
                )
                .into(),
            ),
            Err(err) => tracing::warn!(%err, "could not build the capability registration"),
        }
    }

    fn on_notification(&mut self, notification: Notification) {
        use lsp_types::notification::{
            DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
        };

        let method = notification.method.clone();
        let handled = std::panic::catch_unwind(AssertUnwindSafe(|| {
            match method.as_str() {
                DidOpenTextDocument::METHOD => self
                    .extract(notification, |this, p: lsp_types::DidOpenTextDocumentParams| {
                        this.did_open(p)
                    }),
                DidChangeTextDocument::METHOD => self
                    .extract(notification, |this, p: lsp_types::DidChangeTextDocumentParams| {
                        this.did_change(p)
                    }),
                DidCloseTextDocument::METHOD => self
                    .extract(notification, |this, p: lsp_types::DidCloseTextDocumentParams| {
                        this.did_close(p)
                    }),
                DidSaveTextDocument::METHOD => self
                    .extract(notification, |this, p: lsp_types::DidSaveTextDocumentParams| {
                        this.did_save(p)
                    }),
                // Notifications get no response, so an unknown one is only worth
                // a log line -- but it is worth one, since a silently ignored
                // `didChange` looks exactly like a client that stopped typing.
                other => tracing::debug!(method = other, "ignoring notification"),
            }
        }));
        if handled.is_err() {
            tracing::error!(method, "notification handler panicked");
        }
    }

    /// Deserializes a notification's params and runs `f`, logging rather than
    /// failing when the client sends something unexpected.
    fn extract<P: serde::de::DeserializeOwned>(
        &mut self,
        notification: Notification,
        f: impl FnOnce(&mut Server, P),
    ) {
        let method = notification.method.clone();
        match serde_json::from_value::<P>(notification.params) {
            Ok(params) => f(self, params),
            Err(err) => tracing::warn!(%method, %err, "malformed notification params"),
        }
    }

    fn did_open(&mut self, params: lsp_types::DidOpenTextDocumentParams) {
        let doc = params.text_document;
        let Some(path) = self.resolve(&doc.uri) else { return };

        let differs_from_disk = path
            .as_real()
            .and_then(|abs| std::fs::read_to_string(abs.as_str()).ok())
            .is_none_or(|disk| disk != doc.text);
        if differs_from_disk {
            self.stale_documents.insert(path.clone());
        }

        if self.documents.open(path.clone(), doc.version, doc.text.clone()).is_some() {
            tracing::warn!(uri = %doc.uri, "didOpen for an already-open document");
        }
        self.record_vfs_contents(path, Some(doc.text.into_bytes()));
        // At info, because "did the server even see my file" is the first
        // question anyone asks, and a debug-level answer means turning the log
        // up and reproducing.
        tracing::info!(
            uri = %doc.uri,
            indexed = self.index.as_ref().is_some_and(|i| !i.is_empty()),
            open = self.documents.len(),
            "opened"
        );
    }

    fn did_change(&mut self, params: lsp_types::DidChangeTextDocumentParams) {
        let Some(path) = self.resolve(&params.text_document.uri) else { return };

        let version = params.text_document.version;
        let Some(text) =
            self.documents.apply_changes(&path, version, &params.content_changes, self.encoding)
        else {
            tracing::warn!(uri = %params.text_document.uri, "didChange for a document that is not open");
            return;
        };
        let bytes = text.as_bytes().to_vec();
        if self.record_vfs_contents(path.clone(), Some(bytes)) {
            self.stale_documents.insert(path);
        }
    }

    fn did_save(&mut self, params: lsp_types::DidSaveTextDocumentParams) {
        // A save makes disk and the buffer agree. The built index still
        // describes the previous build and remains stale until a validated
        // shard generation is installed.
        tracing::debug!(uri = %params.text_document.uri, "saved; awaiting index rebuild");
    }

    fn did_close(&mut self, params: lsp_types::DidCloseTextDocumentParams) {
        let Some(path) = self.resolve(&params.text_document.uri) else { return };

        if self.documents.close(&path).is_none() {
            tracing::warn!(uri = %params.text_document.uri, "didClose for a document that was not open");
            return;
        }
        // The client's copy is gone, so disk is authoritative again. Leaving the
        // in-memory text in the VFS would keep serving edits the user discarded.
        let on_disk = path.as_real().and_then(|abs| std::fs::read(abs.as_str()).ok());
        self.record_vfs_contents(path, on_disk);
        // Logged at the same level as `opened`, because without it the open
        // count appears to fall between two consecutive opens.
        tracing::info!(
            uri = %params.text_document.uri,
            open = self.documents.len(),
            "closed"
        );
    }

    /// Resolves a client URI, logging and skipping anything unusable.
    fn resolve(&self, url: &lsp_types::Url) -> Option<VfsPath> {
        match uri::vfs_path(url) {
            Ok(path) => Some(path),
            Err(err) => {
                tracing::debug!(%url, %err, "ignoring document");
                None
            }
        }
    }

    /// Updates VFS identity/revision while releasing the unused change payload.
    ///
    /// No database consumes these batches yet. Retaining them kept one complete
    /// byte buffer for every file visited during the session. Freshness is
    /// tracked explicitly in `stale_documents` instead.
    fn record_vfs_contents(&mut self, path: VfsPath, contents: Option<Vec<u8>>) -> bool {
        let changed = self.vfs.set_file_contents(path, contents);
        drop(self.vfs.take_changes());
        changed
    }

    fn status(&self) -> Status {
        Status {
            workspace_root: self.workspace_root.as_ref().map(|r| r.as_str().to_owned()),
            build_graph_available: self.build.is_some(),
            position_encoding: match self.encoding {
                PositionEncoding::Utf8 => "utf-8",
                PositionEncoding::Utf16 => "utf-16",
            },
            index_loaded: self.index.is_some(),
            watching: self.watcher.is_some(),
            index_cache_loaded: self.cache_hit,
            shards_verified: self.shards_verified,
            shard_refresh_mode: if self.refresh_intent == RefreshIntent::AwaitingBuild {
                "awaiting-build"
            } else if self.cache.is_some() {
                "periodic"
            } else if self.refresh_intent == RefreshIntent::Retry {
                "retry"
            } else if self.watcher.is_some() {
                "watch"
            } else {
                "none"
            },
            output_base: self.config.output_base.as_ref().map(|p| p.to_string()),
            index_targets: self.config.index.targets.clone(),
            indexed_definitions: self.index.as_ref().map(|i| i.definition_count()).unwrap_or(0),
            open_documents: self.documents.len(),
            stale_documents: self.stale_documents.len(),
            vfs_files: self.vfs.len(),
            vfs_revision: self.vfs.revision().as_u64(),
            pending_changes: self.vfs.has_pending_changes(),
            health: self.telemetry.health(),
        }
    }

    fn send(&self, message: Message) {
        // A closed channel means the client is gone; the loop will notice.
        if self.sender.send(message).is_err() {
            tracing::debug!("client channel closed");
        }
    }
}

/// The payload of [`STATUS_REQUEST`].
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub workspace_root: Option<String>,
    pub build_graph_available: bool,
    pub position_encoding: &'static str,
    pub index_loaded: bool,
    pub watching: bool,
    /// True when startup loaded the persisted built-index snapshot.
    pub index_cache_loaded: bool,
    /// Whether the snapshot was compared with the current shard metadata.
    /// This says nothing about whether sources have been rebuilt.
    pub shards_verified: bool,
    pub shard_refresh_mode: &'static str,
    /// `null` when sharing the workspace's default output base.
    pub output_base: Option<String>,
    pub index_targets: Vec<String>,
    pub indexed_definitions: usize,
    pub open_documents: usize,
    pub stale_documents: usize,
    pub vfs_files: usize,
    pub vfs_revision: u64,
    /// True when an unconsumed VFS payload remains. The server currently drains
    /// these after every edit because no database consumes them yet.
    pub pending_changes: bool,
    pub health: telemetry::Health,
}

#[cfg(test)]
mod startup_cache_tests {
    use super::*;
    use symbol_index::{Definition, PositionEncoding as IndexEncoding, Range, SymbolKind};

    fn test_index() -> SymbolIndex {
        let mut index = SymbolIndex::default();
        index.insert(Definition {
            symbol: "java Foo#".into(),
            name: "Foo".into(),
            kind: SymbolKind::Class,
            path: "src/Foo.java".into(),
            range: Range { start_line: 0, start_col: 0, end_line: 0, end_col: 3 },
            encoding: IndexEncoding::Utf16,
            implements: Vec::new(),
            documentation: Vec::new(),
            signature: String::new(),
            enclosing: None,
        });
        index
    }

    fn server() -> Server {
        let (connection, _client) = Connection::memory();
        Server::new(connection.sender, PositionEncoding::Utf16, None)
    }

    fn server_at(root: &Path) -> Server {
        let (connection, _client) = Connection::memory();
        let root = AbsPathBuf::try_from(root.to_str().expect("UTF-8 test path")).unwrap();
        Server::new(connection.sender, PositionEncoding::Utf16, Some(root))
    }

    #[test]
    fn startup_uses_built_snapshot_before_scanning_shards() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("bazel-bin");
        std::fs::create_dir(&dir).unwrap();
        // No SCIP shard exists. Only a successful cache read can discover Foo.
        let index = test_index();
        let config = Config::default();
        let key = CacheKey::new(temp.path(), &dir, &config).unwrap();
        index_cache::store(temp.path(), &dir, &key, &[], &index, || true).unwrap();
        let root = AbsPath::try_new(paths::Utf8Path::new(temp.path().to_str().unwrap())).unwrap();
        let discovered = discover_index(root, &config).unwrap();
        assert!(discovered.cache_hit);
        assert_eq!(discovered.index.search("Foo").len(), 1);
    }

    #[test]
    fn a_refresh_can_restore_an_index_after_invalidation() {
        let mut server = server();
        server.on_refresh_result(RefreshResult {
            revision: 0,
            result: Ok(RefreshWork::Loaded(
                Box::new(test_index()),
                vec![ShardMetadata { path: "foo.scip".into(), len: 1, modified_ns: 1 }],
            )),
        });
        assert_eq!(server.index.as_ref().unwrap().search("Foo").len(), 1);
    }

    #[test]
    fn a_confirmed_empty_shard_set_clears_stale_symbols() {
        let mut server = server();
        server.index = Some(Arc::new(test_index()));
        server.on_refresh_result(RefreshResult {
            revision: 0,
            result: Ok(RefreshWork::Empty(Vec::new())),
        });
        assert!(server.index.is_none());
        assert!(server.shards_verified);
    }

    #[test]
    fn a_timer_tick_does_not_cancel_a_long_refresh() {
        let mut server = server();
        server.refresh_running = true;
        server.refresh_revision = 7;
        server.schedule_refresh(false, || Ok(RefreshWork::Unchanged));
        assert_eq!(server.refresh_revision, 7);
        assert!(!server.refresh_again);

        server.schedule_refresh(true, || Ok(RefreshWork::Unchanged));
        assert_eq!(server.refresh_revision, 8);
        assert!(server.refresh_again);
    }

    #[test]
    fn a_complete_uncached_index_is_not_reloaded_on_a_timer() {
        let temp = tempfile::tempdir().unwrap();
        let mut server = server();
        server.index = Some(Arc::new(test_index()));
        server.index_dir = Some(paths::Utf8PathBuf::from(temp.path().to_str().unwrap().to_owned()));
        server.index_shards = Some(Vec::new());
        server.shards_verified = true;
        server.refresh_intent = RefreshIntent::Idle;

        server.on_refresh_tick();

        assert!(!server.refresh_running);
        assert!(server.index.is_some());
    }

    #[test]
    fn workspace_movement_waits_for_a_new_shard_event() {
        let temp = tempfile::tempdir().unwrap();
        let mut server = server();
        server.index = Some(Arc::new(test_index()));
        server.index_dir = Some(paths::Utf8PathBuf::from(temp.path().to_str().unwrap().to_owned()));
        server.index_shards = Some(Vec::new());
        server.shards_verified = true;

        server.on_file_change(Change::Workspace);
        assert!(server.index.is_none());
        assert_eq!(server.refresh_intent, RefreshIntent::AwaitingBuild);

        server.on_refresh_tick();
        assert!(!server.refresh_running, "a timer is not evidence of a new build");
        server.on_file_change(Change::WatcherError);
        assert!(!server.refresh_running, "watcher uncertainty is not evidence of a new build");
        assert_eq!(server.refresh_intent, RefreshIntent::AwaitingBuild);

        server.on_file_change(Change::Index);
        assert!(server.refresh_running, "a shard event permits a validated reload");
        assert_eq!(server.refresh_intent, RefreshIntent::Retry);
    }

    #[test]
    fn a_watcher_error_validates_and_keeps_an_unchanged_index() {
        let temp = tempfile::tempdir().unwrap();
        let mut server = server();
        server.index = Some(Arc::new(test_index()));
        server.index_dir = Some(paths::Utf8PathBuf::from(temp.path().to_str().unwrap().to_owned()));
        server.index_shards = Some(Vec::new());
        server.shards_verified = true;

        server.on_file_change(Change::WatcherError);
        assert!(server.index.is_some(), "uncertainty must not discard a healthy index");
        assert!(!server.shards_verified);

        let result = server.refresh_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        server.on_refresh_result(result);
        assert!(server.index.is_some());
        assert!(server.shards_verified);
        assert_eq!(server.refresh_intent, RefreshIntent::Idle);
    }

    #[test]
    fn positional_queries_refuse_source_newer_than_the_index() {
        let temp = tempfile::tempdir().unwrap();
        let mut server = server_at(temp.path());
        server.index = Some(Arc::new(test_index()));
        let root = server.workspace_root.as_ref().unwrap();
        let abs = root.join("src/Foo.java");
        server.stale_documents.insert(VfsPath::Real(abs.clone()));
        let uri = lsp_types::Url::from_file_path(abs.as_str()).unwrap();

        let position = || {
            serde_json::json!({
                "textDocument": { "uri": uri },
                "position": { "line": 0, "character": 0 }
            })
        };
        let assert_stale = |error: RequestError| {
            assert!(matches!(error.code, ErrorCode::ContentModified));
            assert!(error.message.contains("rebuild the index"));
        };

        assert_stale(server.goto_definition(position()).unwrap_err());
        assert_stale(
            server
                .find_references(serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": 0, "character": 0 },
                    "context": { "includeDeclaration": true }
                }))
                .err()
                .expect("stale reference query must fail"),
        );
        assert_stale(server.hover(position()).unwrap_err());
        assert_stale(server.goto_implementation(position()).unwrap_err());
        assert_stale(server.prepare_call_hierarchy(position()).unwrap_err());

        let item = || {
            serde_json::json!({
                "item": {
                    "name": "Foo",
                    "kind": 12,
                    "uri": uri,
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 3 }
                    },
                    "selectionRange": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 3 }
                    },
                    "data": { "symbol": "java Foo#" }
                }
            })
        };
        assert_stale(server.call_hierarchy(item(), CallDirection::Incoming).unwrap_err());
        assert_stale(server.call_hierarchy(item(), CallDirection::Outgoing).unwrap_err());
    }

    #[test]
    fn freshness_survives_save_and_close_while_vfs_payloads_are_drained() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("A.java");
        std::fs::write(&file, "class A {}\n").unwrap();
        let uri = lsp_types::Url::from_file_path(&file).unwrap();
        let path = uri::vfs_path(&uri).unwrap();
        let mut server = server_at(temp.path());

        server.did_open(
            serde_json::from_value(serde_json::json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "java",
                    "version": 1,
                    "text": "class A {}\n"
                }
            }))
            .unwrap(),
        );
        assert!(!server.stale_documents.contains(&path));
        assert!(!server.vfs.has_pending_changes());

        server.did_change(
            serde_json::from_value(serde_json::json!({
                "textDocument": { "uri": uri, "version": 2 },
                "contentChanges": [{ "text": "class Bee {}\n" }]
            }))
            .unwrap(),
        );
        assert!(server.stale_documents.contains(&path));
        assert!(!server.vfs.has_pending_changes());

        std::fs::write(&file, "class Bee {}\n").unwrap();
        server.did_save(
            serde_json::from_value(serde_json::json!({
                "textDocument": { "uri": uri }
            }))
            .unwrap(),
        );
        server.did_close(
            serde_json::from_value(serde_json::json!({
                "textDocument": { "uri": uri }
            }))
            .unwrap(),
        );
        assert!(server.stale_documents.contains(&path), "saving is not an index rebuild");
        assert!(!server.vfs.has_pending_changes());

        // Installing a generation built from disk makes the closed file current.
        server.refresh_document_freshness();
        assert!(!server.stale_documents.contains(&path));
    }

    #[test]
    fn a_queued_reload_runs_after_invalidation() {
        let temp = tempfile::tempdir().unwrap();
        let mut server = server();
        server.index_dir = Some(paths::Utf8PathBuf::from(temp.path().to_str().unwrap().to_owned()));
        server.refresh_revision = 2;
        server.refresh_running = true;
        server.refresh_again = true;

        // Revision 1 is the first worker, made obsolete by a second shard
        // notification. Its completion must launch the queued worker even
        // though workspace invalidation already cleared the index.
        server.on_refresh_result(RefreshResult { revision: 1, result: Ok(RefreshWork::Unchanged) });
        assert!(server.refresh_running);
        assert!(!server.refresh_again);
        assert_eq!(server.refresh_revision, 3);
    }
}

/// Both shapes of a references answer: the LSP-conformant array, and the
/// fuller reply that names what was withheld.
struct ReferenceReply {
    locations: serde_json::Value,
    full: serde_json::Value,
}

impl ReferenceReply {
    fn new(
        symbol: &str,
        locations: Vec<lsp_types::Location>,
        total: usize,
    ) -> Result<ReferenceReply, RequestError> {
        let returned = locations.len();
        let locations = serde_json::to_value(locations)?;
        let full = serde_json::json!({
            "symbol": symbol,
            "returned": returned,
            "total": total,
            "truncated": returned < total,
            "locations": locations,
        });
        Ok(ReferenceReply { locations, full })
    }
}

/// Which way a call hierarchy is being walked.
#[derive(Copy, Clone)]
enum CallDirection {
    Incoming,
    Outgoing,
}

/// A method with no callers is a true answer, not a failure — an entry point,
/// or dead code, which is often exactly what the caller wanted to learn.
fn count_outcome(count: usize) -> telemetry::Outcome {
    if count == 0 {
        telemetry::Outcome::Empty { reason: telemetry::EmptyReason::NoMatch }
    } else {
        telemetry::Outcome::answered(count)
    }
}

/// Reads a workspace-relative file, preferring the client's open copy.
///
/// The client's buffer is authoritative for an open file and differs from disk
/// from the first keystroke until a save. A free function rather than a method
/// so it borrows only the documents, leaving the rest of the server mutable.
fn file_reader<'a>(
    documents: &'a Documents,
    root: &'a AbsPathBuf,
) -> impl Fn(&str) -> Option<String> + 'a {
    move |relative: &str| {
        let abs = root.join(relative);
        documents
            .get(&VfsPath::Real(abs.clone()))
            .map(|doc| doc.text.clone())
            .or_else(|| std::fs::read_to_string(abs.as_str()).ok())
    }
}

/// Records a failure on an in-flight guard that is about to be dropped.
fn guard_failed(guard: &mut telemetry::InFlight<'_>) {
    guard.mark_failed(telemetry::Failure::Io);
}

#[derive(Debug)]
struct RequestError {
    code: ErrorCode,
    message: String,
}

impl RequestError {
    fn new(code: ErrorCode, message: String) -> RequestError {
        RequestError { code, message }
    }
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl From<serde_json::Error> for RequestError {
    fn from(err: serde_json::Error) -> RequestError {
        RequestError::new(ErrorCode::InternalError, err.to_string())
    }
}
