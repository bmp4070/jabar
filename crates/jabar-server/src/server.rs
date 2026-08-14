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
use paths::AbsPathBuf;
use serde::Serialize;
use telemetry::Telemetry;
use vfs::{Vfs, VfsPath};

use crate::capabilities::{negotiate_encoding, server_capabilities, workspace_root};
use crate::documents::Documents;
use crate::handlers;
use crate::line_index::PositionEncoding;
use crate::uri;
use symbol_index::SymbolIndex;

/// Custom request: what the server currently believes about itself.
///
/// Not part of LSP. It exists because "is the language server working" is
/// otherwise unanswerable from outside, and for an agent client a server that is
/// quietly answering nothing looks exactly like a codebase with nothing in it.
pub const STATUS_REQUEST: &str = "jabar/status";

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

    let result = lsp_types::InitializeResult {
        capabilities: server_capabilities(encoding),
        server_info: Some(lsp_types::ServerInfo {
            name: "jabar".to_owned(),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        }),
    };
    connection
        .initialize_finish(id, serde_json::to_value(result)?)
        .context("initialize handshake failed")?;

    Server::new(connection.sender.clone(), encoding, root).run(&connection)
}

pub struct Server {
    sender: Sender<Message>,
    encoding: PositionEncoding,
    workspace_root: Option<AbsPathBuf>,
    /// Present once a workspace root is known. Queries against it come later.
    build: Option<BazelCli>,
    vfs: Vfs,
    documents: Documents,
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
        for message in &connection.receiver {
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
        // The channel closed without `exit`. Common enough when an editor is
        // killed, and not worth failing over.
        tracing::info!("client disconnected");
        Ok(())
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
            // Refusing loudly matters: silently returning null would look to a
            // client like a successful empty answer.
            unknown => Err(RequestError::new(
                ErrorCode::MethodNotFound,
                format!("`{unknown}` is not implemented"),
            )),
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

        let documents = &self.documents;
        let results =
            handlers::workspace_symbol(index, &params.query, root, self.encoding, |relative| {
                // Prefer the client's copy, which is authoritative for an open
                // file and may differ from what the index was built against.
                let abs = root.join(relative);
                let path = VfsPath::Real(abs.clone());
                documents
                    .get(&path)
                    .map(|doc| doc.text.clone())
                    .or_else(|| std::fs::read_to_string(abs.as_str()).ok())
            });

        guard.finish(results.outcome());
        Ok(serde_json::to_value(results.symbols)?)
    }

    /// Registers `workspace/symbol` dynamically, now that it can be served.
    fn register_workspace_symbol(&mut self) {
        let registration = lsp_types::Registration {
            id: "jabar-workspace-symbol".to_owned(),
            method: lsp_types::request::WorkspaceSymbolRequest::METHOD.to_owned(),
            register_options: None,
        };
        let params = lsp_types::RegistrationParams { registrations: vec![registration] };
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
    pub indexed_definitions: usize,
    pub open_documents: usize,
    pub vfs_files: usize,
    pub vfs_revision: u64,
    /// True when writes are waiting to reach the database, which is the
    /// read-after-write signal from `docs/phase-1.md` (F3).
    pub pending_changes: bool,
    pub health: telemetry::Health,
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
