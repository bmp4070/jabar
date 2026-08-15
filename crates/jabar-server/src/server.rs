//! Server state and the main loop.
//!
//! One thread, one owner of the state. There is no task pool yet because there
//! is nothing expensive to run on it: every handler here is bookkeeping. The
//! pool arrives with the first query that touches the database, and with it the
//! snapshot split rust-analyzer uses — see `docs/phase-1.md`, which explains why
//! salsa cancellation wants exactly one writer.
//!
//! What the loop guarantees now is the part that is painful to retrofit: every
//! request gets exactly one response, unknown methods are refused rather than
//! ignored, and a handler that panics does not take the session down.

use std::panic::AssertUnwindSafe;

use anyhow::Context as _;
use build_model::BazelCli;
use crossbeam_channel::Sender;
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::notification::Notification as _;
use lsp_types::request::Request as _;
use paths::{AbsPath, AbsPathBuf};
use serde::Serialize;
use telemetry::Telemetry;
use vfs::{Vfs, VfsPath};

use crate::capabilities::{negotiate_encoding, server_capabilities, workspace_root};
use crate::documents::Documents;
use crate::handlers;
use crate::line_index::PositionEncoding;
use crate::uri;
use symbol_index::SymbolIndex;
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
    let root = workspace_root(&params)
        .as_ref()
        .and_then(|url| uri::vfs_path(url).ok())
        .and_then(|path| path.as_real().cloned());

    if let Some(client) = &params.client_info {
        tracing::info!(name = %client.name, version = ?client.version, "client connected");
    }
    tracing::info!(?encoding, root = ?root.as_ref().map(|r| r.as_str()), "initializing");

    // Find an index before advertising, because LSP has no way to say
    // "supported, but not yet": a provider advertised with nothing behind it
    // means clients call it and get nothing, which reads as "no such symbol".
    let discovered = root.as_deref().and_then(discover_index);
    if let Some((dir, shards, definitions)) = &discovered {
        tracing::info!(%dir, shards, definitions, "found an index at startup");
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
    if let Some((dir, _, _)) = discovered {
        server.adopt_index(&dir);
    }
    server.run(&connection)
}

/// Looks for SCIP shards under a workspace.
///
/// Returns the directory, shard count and definition count when it finds any.
/// The conventional home is `bazel-bin`, which is the convenience symlink Bazel
/// writes at the workspace root; shards land one per target beneath it.
///
/// Reads the whole index to answer, which is the only honest way to know it is
/// usable — a directory of unparseable files is not an index. On Gerrit that is
/// 97 shards and 343ms, paid once at startup.
fn discover_index(root: &AbsPath) -> Option<(paths::Utf8PathBuf, usize, usize)> {
    // `bazel-bin` is a symlink into the output base; `symlink_metadata` would
    // see the link rather than the directory, so follow it deliberately.
    for candidate in ["bazel-bin", ".jabar/index"] {
        let dir = root.join(candidate);
        if !dir.as_utf8_path().is_dir() {
            continue;
        }
        match SymbolIndex::from_dir(std::path::Path::new(dir.as_str())) {
            Ok(index) if !index.is_empty() => {
                return Some((
                    dir.into_utf8_path_buf(),
                    index.shard_count(),
                    index.definition_count(),
                ));
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
    index: Option<SymbolIndex>,
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
        Server {
            sender,
            encoding,
            workspace_root,
            build,
            vfs: Vfs::default(),
            documents: Documents::default(),
            index: None,
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
                }
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

    /// Reacts to something changing on disk.
    ///
    /// Reloading is synchronous, which is fine while it is a directory read —
    /// Ray's seven shards load in 132ms. When the index grows, or when jabar
    /// runs the aspect itself, this moves to the task pool.
    fn on_file_change(&mut self, change: Change) {
        match change {
            Change::Index => {
                tracing::info!("shards changed on disk; reloading the index");
                self.reload_index();
            }
            Change::Workspace => {
                // A branch switch invalidates the index without necessarily
                // rewriting any shard: the shards on disk now describe the old
                // tree. Nothing can be reloaded that would be right, so the
                // honest move is to drop the index and say so.
                tracing::info!("the workspace moved; dropping the index as stale");
                self.index = None;
                self.notify_index_stale();
            }
        }
    }

    fn reload_index(&mut self) {
        let Some(dir) = self.index_dir.clone() else { return };
        let mut guard = self.telemetry.start(telemetry::Op::IndexBuild);
        match SymbolIndex::from_dir(std::path::Path::new(dir.as_str())) {
            Ok(index) => {
                let definitions = index.definition_count();
                guard.finish(telemetry::Outcome::answered(definitions));
                tracing::info!(shards = index.shard_count(), definitions, "index reloaded");
                self.index = Some(index);
            }
            Err(err) => {
                guard.mark_failed(telemetry::Failure::Io);
                tracing::warn!(%err, dir = %dir, "could not reload the index; keeping the old one");
            }
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
            // Refusing loudly matters: silently returning null would look to a
            // client like a successful empty answer.
            unknown => Err(RequestError::new(
                ErrorCode::MethodNotFound,
                format!("`{unknown}` is not implemented"),
            )),
        }
    }

    /// Takes on an index discovered at startup.
    ///
    /// Re-reads rather than taking the one `discover_index` built, because that
    /// one was read before the server existed and threading it through would
    /// complicate the constructor for a one-off saving of a few hundred
    /// milliseconds.
    pub fn adopt_index(&mut self, dir: &paths::Utf8Path) {
        match SymbolIndex::from_dir(std::path::Path::new(dir.as_str())) {
            Ok(index) => {
                self.index = Some(index);
                self.index_dir = Some(dir.to_path_buf());
                self.start_watching(dir.as_str());
            }
            Err(err) => tracing::warn!(%err, %dir, "could not adopt the index found at startup"),
        }
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
        let index = SymbolIndex::from_dir(std::path::Path::new(&params.path)).map_err(|err| {
            guard_failed(&mut guard);
            RequestError::new(
                ErrorCode::InvalidParams,
                format!("could not read `{}`: {err}", params.path),
            )
        })?;

        let (shards, definitions) = (index.shard_count(), index.definition_count());
        guard.finish(if definitions == 0 {
            telemetry::Outcome::Empty { reason: telemetry::EmptyReason::NoMatch }
        } else {
            telemetry::Outcome::answered(definitions)
        });

        tracing::info!(shards, definitions, path = %params.path, "index loaded");
        self.index = Some(index);
        self.index_dir = Some(paths::Utf8PathBuf::from(params.path.clone()));
        self.start_watching(&params.path);
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
        let (Some(index), Some(root)) = (&self.index, &self.workspace_root) else {
            guard.finish(handlers::index_unavailable_outcome());
            return Err(RequestError::new(
                ErrorCode::ServerNotInitialized,
                "no symbol index is loaded; run the SCIP aspect and call `jabar/loadIndex`"
                    .to_owned(),
            ));
        };

        let read = file_reader(&self.documents, root);
        let results = handlers::workspace_symbol(index, &params.query, root, self.encoding, read);

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
        guard.mark_stale(self.vfs.has_pending_changes());

        let Some((index, root, relative)) = self.resolve_query(&doc.text_document.uri) else {
            let err = self.refuse(&mut guard, &doc.text_document.uri);
            return Err(err);
        };
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
                // define -- a JDK or third-party type, whose definition lives in
                // a jar no shard covers. Both are an honest "no match".
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
        guard.mark_stale(self.vfs.has_pending_changes());

        let Some((index, root, relative)) = self.resolve_query(&doc.text_document.uri) else {
            let err = self.refuse(&mut guard, &doc.text_document.uri);
            return Err(err);
        };
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
        guard.mark_stale(self.vfs.has_pending_changes());

        let Some((index, root, relative)) = self.resolve_query(&doc.text_document.uri) else {
            let err = self.refuse(&mut guard, &doc.text_document.uri);
            return Err(err);
        };
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
        guard.mark_stale(self.vfs.has_pending_changes());

        let Some((index, root, relative)) = self.resolve_query(&doc.text_document.uri) else {
            let err = self.refuse(&mut guard, &doc.text_document.uri);
            return Err(err);
        };
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
        guard.mark_stale(self.vfs.has_pending_changes());

        let Some((index, root, relative)) = self.resolve_query(&params.text_document.uri) else {
            let err = self.refuse(&mut guard, &params.text_document.uri);
            return Err(err);
        };
        let read = file_reader(&self.documents, root);
        let symbols = handlers::document_symbols(index, &relative, self.encoding, &read);

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
    fn start_watching(&mut self, index_dir: &str) {
        let dir = paths::Utf8Path::new(index_dir);
        let index_dir = paths::AbsPath::try_new(dir);
        match FileWatcher::spawn(index_dir, self.workspace_root.as_deref()) {
            Ok(watcher) => {
                tracing::debug!(dir = %dir, "watching for index changes");
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
                DidSaveTextDocument::METHOD => {
                    self.extract(notification, |_this, p: lsp_types::DidSaveTextDocumentParams| {
                        // Disk and the in-memory copy now agree, so there is
                        // nothing to reconcile -- the client keeps the document
                        // open and its text is unchanged by the save.
                        tracing::debug!(uri = %p.text_document.uri, "saved");
                    })
                }
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

        if self.documents.open(path.clone(), doc.version, doc.text.clone()).is_some() {
            tracing::warn!(uri = %doc.uri, "didOpen for an already-open document");
        }
        self.vfs.set_file_contents(path, Some(doc.text.into_bytes()));
        tracing::debug!(uri = %doc.uri, open = self.documents.len(), "opened");
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
        self.vfs.set_file_contents(path, Some(bytes));
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
        self.vfs.set_file_contents(path, on_disk);
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
            indexed_definitions: self.index.as_ref().map(|i| i.definition_count()).unwrap_or(0),
            open_documents: self.documents.len(),
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
    pub indexed_definitions: usize,
    pub open_documents: usize,
    pub vfs_files: usize,
    pub vfs_revision: u64,
    /// True when writes are waiting to reach the database, which is the
    /// read-after-write signal from `docs/phase-1.md` (F3).
    pub pending_changes: bool,
    pub health: telemetry::Health,
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
