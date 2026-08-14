//! Drives a real LSP session over an in-memory connection.
//!
//! These exercise the wire protocol rather than any handler's logic: that every
//! request gets exactly one response, that the lifecycle sequence is honoured,
//! and that a client sending nonsense does not take the server down. Those are
//! the properties that are painful to retrofit and easy to break.

use std::thread;

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::Notification as _;
use lsp_types::request::Request as _;
use serde_json::{Value, json};

/// A client talking to a server running on its own thread.
struct Harness {
    client: Connection,
    server: Option<thread::JoinHandle<anyhow::Result<()>>>,
    next_id: i32,
    /// The `InitializeResult` the server replied with.
    initialized: Value,
}

impl Harness {
    /// Starts a server and completes the `initialize` handshake.
    ///
    /// `client_capabilities` is passed through verbatim so tests can control
    /// encoding negotiation.
    fn start(root: Option<&str>, client_capabilities: Value) -> Harness {
        let (server_conn, client_conn) = Connection::memory();
        let server = thread::spawn(move || jabar_server::run_server(server_conn));

        let mut harness = Harness {
            client: client_conn,
            server: Some(server),
            next_id: 0,
            initialized: Value::Null,
        };

        let params = json!({
            "processId": null,
            "clientInfo": { "name": "harness", "version": "1.0" },
            "capabilities": client_capabilities,
            "workspaceFolders": root.map(|r| json!([{ "uri": r, "name": "root" }])),
        });
        let id = harness.send_request(lsp_types::request::Initialize::METHOD, params);
        let result = harness.expect_ok(id);
        harness.send_notification(lsp_types::notification::Initialized::METHOD, json!({}));

        harness.initialized = result;
        harness
    }

    fn capabilities(&self) -> &Value {
        &self.initialized["capabilities"]
    }

    fn send_request(&mut self, method: &str, params: Value) -> RequestId {
        self.next_id += 1;
        let id = RequestId::from(self.next_id);
        let request = Request { id: id.clone(), method: method.to_owned(), params };
        self.client.sender.send(Message::Request(request)).expect("server should be listening");
        id
    }

    fn send_notification(&self, method: &str, params: Value) {
        let notification = Notification { method: method.to_owned(), params };
        self.client
            .sender
            .send(Message::Notification(notification))
            .expect("server should be listening");
    }

    /// Waits for the response to `id`, failing on anything else.
    fn recv_response(&self, id: &RequestId) -> Response {
        loop {
            match self.client.receiver.recv().expect("server closed the connection") {
                Message::Response(response) => {
                    assert_eq!(&response.id, id, "response arrived for the wrong request");
                    return response;
                }
                // Server-initiated traffic is fine to skip past.
                Message::Notification(_) | Message::Request(_) => continue,
            }
        }
    }

    fn expect_ok(&self, id: RequestId) -> Value {
        let response = self.recv_response(&id);
        assert!(response.error.is_none(), "expected success, got {:?}", response.error);
        response.result.expect("a successful response carries a result")
    }

    fn expect_err(&self, id: RequestId) -> lsp_server::ResponseError {
        let response = self.recv_response(&id);
        assert!(response.result.is_none(), "expected an error, got {:?}", response.result);
        response.error.expect("a failed response carries an error")
    }

    fn status(&mut self) -> Value {
        let id = self.send_request("jabar/status", json!(null));
        self.expect_ok(id)
    }

    /// Runs the shutdown/exit sequence and asserts the server thread ended
    /// cleanly.
    fn shutdown(mut self) {
        let id = self.send_request(lsp_types::request::Shutdown::METHOD, json!(null));
        self.expect_ok(id);
        self.send_notification(lsp_types::notification::Exit::METHOD, json!(null));

        let server = self.server.take().expect("server handle");
        server.join().expect("server thread panicked").expect("server returned an error");
    }
}

fn utf8_client() -> Value {
    json!({ "general": { "positionEncodings": ["utf-8", "utf-16"] } })
}

fn default_client() -> Value {
    json!({})
}

fn open(harness: &Harness, uri: &str, text: &str) {
    harness.send_notification(
        lsp_types::notification::DidOpenTextDocument::METHOD,
        json!({ "textDocument": { "uri": uri, "languageId": "java", "version": 1, "text": text } }),
    );
}

#[test]
fn completes_the_full_lifecycle() {
    let harness = Harness::start(None, default_client());
    harness.shutdown();
}

#[test]
fn negotiates_utf8_when_the_client_offers_it() {
    let harness = Harness::start(None, utf8_client());
    assert_eq!(harness.capabilities()["positionEncoding"], json!("utf-8"));
    harness.shutdown();
}

#[test]
fn falls_back_to_utf16_when_the_client_is_silent() {
    // The protocol default. Guessing utf-8 here would skew every range on any
    // file containing a non-ASCII character.
    let harness = Harness::start(None, default_client());
    assert_eq!(harness.capabilities()["positionEncoding"], json!("utf-16"));
    harness.shutdown();
}

#[test]
fn advertises_only_what_is_implemented() {
    // An advertised capability that returns nothing tells the client "there are
    // none", which it cannot distinguish from the truth.
    let harness = Harness::start(None, utf8_client());
    let caps = harness.capabilities();
    for unimplemented in [
        "documentSymbolProvider",
        "workspaceSymbolProvider",
        "definitionProvider",
        "referencesProvider",
        "hoverProvider",
        "implementationProvider",
        "callHierarchyProvider",
        "completionProvider",
    ] {
        assert!(caps.get(unimplemented).is_none(), "{unimplemented} should not be advertised yet");
    }
    assert!(caps.get("textDocumentSync").is_some(), "text sync is implemented");
    harness.shutdown();
}

#[test]
fn an_unknown_method_is_refused_rather_than_answered_empty() {
    // `hover` is genuinely not implemented. When it is, this test should move
    // to another unimplemented method rather than be deleted -- the property is
    // that jabar never answers a method it does not serve.
    let mut harness = Harness::start(None, utf8_client());
    let id = harness.send_request("textDocument/hover", json!({}));
    let error = harness.expect_err(id);
    assert_eq!(error.code, lsp_server::ErrorCode::MethodNotFound as i32);
    assert!(error.message.contains("textDocument/hover"), "message: {}", error.message);
    harness.shutdown();
}

#[test]
fn an_implemented_query_without_an_index_refuses_rather_than_returning_null() {
    // `definition` is implemented, but there is no index. A null result would
    // read as "no definition exists", which is a different and false claim.
    let mut harness = Harness::start(Some("file:///repo"), utf8_client());
    let id = harness.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": "file:///repo/A.java" },
            "position": { "line": 0, "character": 0 }
        }),
    );
    let error = harness.expect_err(id);
    assert_eq!(error.code, lsp_server::ErrorCode::ServerNotInitialized as i32);
    assert!(error.message.contains("index"), "message: {}", error.message);
    harness.shutdown();
}

#[test]
fn text_sync_reaches_the_vfs() {
    let mut harness = Harness::start(None, utf8_client());
    let before = harness.status();
    assert_eq!(before["openDocuments"], json!(0));
    assert_eq!(before["vfsRevision"], json!(0));

    open(&harness, "file:///repo/A.java", "class A {}\n");
    let after = harness.status();
    assert_eq!(after["openDocuments"], json!(1));
    assert_eq!(after["vfsFiles"], json!(1));
    assert_eq!(after["vfsRevision"], json!(1), "the open should have advanced the revision");
    harness.shutdown();
}

#[test]
fn incremental_edits_are_applied_in_order() {
    let mut harness = Harness::start(None, utf8_client());
    open(&harness, "file:///repo/A.java", "class A {}\n");

    harness.send_notification(
        lsp_types::notification::DidChangeTextDocument::METHOD,
        json!({
            "textDocument": { "uri": "file:///repo/A.java", "version": 2 },
            "contentChanges": [{
                "range": { "start": { "line": 0, "character": 6 },
                           "end":   { "line": 0, "character": 7 } },
                "text": "Bee"
            }],
        }),
    );
    let status = harness.status();
    assert_eq!(status["vfsRevision"], json!(2), "the edit changed the content");
    harness.shutdown();
}

#[test]
fn an_edit_that_changes_nothing_does_not_advance_the_revision() {
    // Agents rewrite files with identical bytes constantly. If that counted as a
    // change, every such write would invalidate the world.
    let mut harness = Harness::start(None, utf8_client());
    open(&harness, "file:///repo/A.java", "class A {}\n");
    let before = harness.status()["vfsRevision"].clone();

    harness.send_notification(
        lsp_types::notification::DidChangeTextDocument::METHOD,
        json!({
            "textDocument": { "uri": "file:///repo/A.java", "version": 2 },
            "contentChanges": [{ "text": "class A {}\n" }],
        }),
    );
    assert_eq!(harness.status()["vfsRevision"], before);
    harness.shutdown();
}

#[test]
fn malformed_params_do_not_kill_the_session() {
    // A notification gets no response, so a client cannot learn it was rejected.
    // The server must stay usable regardless.
    let mut harness = Harness::start(None, utf8_client());
    harness.send_notification(
        lsp_types::notification::DidOpenTextDocument::METHOD,
        json!({ "nonsense": true }),
    );
    assert_eq!(harness.status()["openDocuments"], json!(0), "the bad notification was dropped");
    harness.shutdown();
}

#[test]
fn an_unsupported_uri_scheme_is_ignored_not_fatal() {
    // `untitled:` is a buffer never written to disk. Editors send these.
    let mut harness = Harness::start(None, utf8_client());
    open(&harness, "untitled:Untitled-1", "class Scratch {}");
    assert_eq!(harness.status()["openDocuments"], json!(0));
    harness.shutdown();
}

#[test]
fn requests_after_shutdown_are_refused() {
    // Per the spec, everything after `shutdown` must fail until `exit`.
    let mut harness = Harness::start(None, utf8_client());
    let id = harness.send_request(lsp_types::request::Shutdown::METHOD, json!(null));
    harness.expect_ok(id);

    let id = harness.send_request("jabar/status", json!(null));
    let error = harness.expect_err(id);
    assert_eq!(error.code, lsp_server::ErrorCode::InvalidRequest as i32);

    harness.send_notification(lsp_types::notification::Exit::METHOD, json!(null));
    let server = harness.server.take().expect("server handle");
    server.join().expect("server thread panicked").expect("server returned an error");
}

#[test]
fn the_workspace_root_is_picked_up() {
    let mut harness = Harness::start(Some("file:///repo/workspace"), utf8_client());
    let status = harness.status();
    assert_eq!(status["workspaceRoot"], json!("/repo/workspace"));
    assert_eq!(status["buildGraphAvailable"], json!(true));
    harness.shutdown();
}

#[test]
fn status_reports_a_healthy_server() {
    let mut harness = Harness::start(None, utf8_client());
    let status = harness.status();
    assert_eq!(status["health"]["concerns"], json!([]), "nothing should have tripped a threshold");
    assert!(status["health"]["ops"].is_array());
    harness.shutdown();
}
