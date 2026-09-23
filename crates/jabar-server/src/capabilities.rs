//! What the server tells the client it can do.
//!
//! Advertise implemented operations consistently. While the index loads, their
//! handlers return an explicit retryable error rather than an empty answer.

use lsp_types::{
    OneOf, PositionEncodingKind, SaveOptions, ServerCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextDocumentSyncOptions, TextDocumentSyncSaveOptions,
    WorkspaceFoldersServerCapabilities, WorkspaceServerCapabilities,
};

use crate::line_index::PositionEncoding;

/// Picks the position encoding to use for the session.
///
/// LSP predates any concern for this and specifies UTF-16 as the only encoding,
/// so UTF-16 is the fallback whenever the client says nothing. A client that
/// advertises UTF-8 lets every offset in the server pass through untouched,
/// which removes a whole class of off-by-a-character bug rather than merely
/// handling it — so it is preferred whenever offered.
pub fn negotiate_encoding(client: &lsp_types::ClientCapabilities) -> PositionEncoding {
    let offered = client.general.as_ref().and_then(|g| g.position_encodings.as_ref());
    match offered {
        Some(kinds) if kinds.contains(&PositionEncodingKind::UTF8) => PositionEncoding::Utf8,
        _ => PositionEncoding::Utf16,
    }
}

impl PositionEncoding {
    pub fn to_lsp(self) -> PositionEncodingKind {
        match self {
            PositionEncoding::Utf8 => PositionEncodingKind::UTF8,
            PositionEncoding::Utf16 => PositionEncodingKind::UTF16,
        }
    }
}

/// What the server advertises at `initialize`.
///
/// Readiness is reported by `jabar/status`. Queries before readiness fail
/// explicitly, so clients can retry without treating an empty result as truth.
pub fn server_capabilities(encoding: PositionEncoding) -> ServerCapabilities {
    // Static advertisement is what makes jabar work with every client rather
    // than only those supporting `client/registerCapability`. Each field has its
    // own `OneOf<bool, _>` type, so the repetition is unavoidable.
    ServerCapabilities {
        position_encoding: Some(encoding.to_lsp()),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        definition_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        implementation_provider: Some(lsp_types::ImplementationProviderCapability::Simple(true)),
        hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),
        call_hierarchy_provider: Some(lsp_types::CallHierarchyServerCapability::Simple(true)),
        // Incremental sync, because the alternative is resending whole files on
        // every keystroke and this server is meant for large ones.
        text_document_sync: Some(TextDocumentSyncCapability::Options(TextDocumentSyncOptions {
            open_close: Some(true),
            change: Some(TextDocumentSyncKind::INCREMENTAL),
            will_save: Some(false),
            will_save_wait_until: Some(false),
            // Save notifications without the text: the file is on disk by then,
            // and shipping the contents again would double every write.
            save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                include_text: Some(false),
            })),
        })),
        workspace: Some(WorkspaceServerCapabilities {
            workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                supported: Some(true),
                change_notifications: Some(OneOf::Left(true)),
            }),
            file_operations: None,
        }),
        ..ServerCapabilities::default()
    }
}

/// The workspace root, taken from whichever of the several places the client
/// might have put it.
///
/// `root_uri` is deprecated in favour of `workspace_folders`, but plenty of
/// clients still send only the former, so both are read.
pub fn workspace_root(params: &lsp_types::InitializeParams) -> Option<lsp_types::Url> {
    if let Some(folders) = &params.workspace_folders
        && let Some(first) = folders.first()
    {
        return Some(first.uri.clone());
    }
    #[allow(deprecated)]
    params.root_uri.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{ClientCapabilities, GeneralClientCapabilities};

    fn client_offering(kinds: Option<Vec<PositionEncodingKind>>) -> ClientCapabilities {
        ClientCapabilities {
            general: Some(GeneralClientCapabilities {
                position_encodings: kinds,
                ..GeneralClientCapabilities::default()
            }),
            ..ClientCapabilities::default()
        }
    }

    #[test]
    fn utf8_is_preferred_when_offered() {
        let client =
            client_offering(Some(vec![PositionEncodingKind::UTF16, PositionEncodingKind::UTF8]));
        assert_eq!(negotiate_encoding(&client), PositionEncoding::Utf8);
    }

    #[test]
    fn utf16_is_the_fallback() {
        // Three ways a client can decline to choose. All must land on UTF-16,
        // because that is what the protocol means by default -- guessing UTF-8
        // here would skew every range on any file with a non-ASCII character.
        assert_eq!(negotiate_encoding(&ClientCapabilities::default()), PositionEncoding::Utf16);
        assert_eq!(negotiate_encoding(&client_offering(None)), PositionEncoding::Utf16);
        assert_eq!(
            negotiate_encoding(&client_offering(Some(vec![PositionEncodingKind::UTF16]))),
            PositionEncoding::Utf16
        );
    }

    #[test]
    fn an_unknown_encoding_does_not_win() {
        let client = client_offering(Some(vec![PositionEncodingKind::UTF32]));
        assert_eq!(negotiate_encoding(&client), PositionEncoding::Utf16);
    }

    #[test]
    fn the_negotiated_encoding_is_reported_back() {
        for encoding in [PositionEncoding::Utf8, PositionEncoding::Utf16] {
            let caps = server_capabilities(encoding);
            assert_eq!(caps.position_encoding, Some(encoding.to_lsp()));
        }
    }

    #[test]
    fn implemented_queries_are_always_advertised() {
        let caps = server_capabilities(PositionEncoding::Utf8);
        assert!(caps.definition_provider.is_some());
        assert!(caps.references_provider.is_some());
        assert!(caps.hover_provider.is_some());
        assert!(caps.implementation_provider.is_some());
        assert!(caps.document_symbol_provider.is_some());
        assert!(caps.workspace_symbol_provider.is_some());
        assert!(caps.call_hierarchy_provider.is_some());
    }

    #[test]
    fn nothing_unimplemented_is_advertised() {
        // The point of this test is to fail loudly when a capability is switched
        // on before its handler exists. An advertised capability that returns
        // nothing tells the client "there are none", which is a lie it cannot
        // detect.
        let caps = server_capabilities(PositionEncoding::Utf8);
        // Everything in the client's surface is now served, so this test has
        // nothing left to guard. It stays as the place to add the next
        // unimplemented capability rather than being deleted.
        assert!(caps.completion_provider.is_none(), "completion is deferred, not built");
        // Cut from the roadmap entirely, not merely pending.
        assert!(caps.completion_provider.is_none(), "completion is out of scope");
        assert!(caps.signature_help_provider.is_none(), "signature help is out of scope");
        assert!(caps.semantic_tokens_provider.is_none(), "semantic tokens are out of scope");
    }

    #[test]
    fn text_sync_is_incremental_and_two_way() {
        let caps = server_capabilities(PositionEncoding::Utf8);
        let Some(TextDocumentSyncCapability::Options(sync)) = caps.text_document_sync else {
            panic!("expected sync options");
        };
        assert_eq!(sync.open_close, Some(true));
        assert_eq!(sync.change, Some(TextDocumentSyncKind::INCREMENTAL));
    }

    #[test]
    fn the_workspace_root_comes_from_folders_first() {
        let folder = lsp_types::WorkspaceFolder {
            uri: lsp_types::Url::parse("file:///repo").unwrap(),
            name: "repo".to_owned(),
        };
        #[allow(deprecated)]
        let params = lsp_types::InitializeParams {
            workspace_folders: Some(vec![folder]),
            root_uri: Some(lsp_types::Url::parse("file:///stale").unwrap()),
            ..lsp_types::InitializeParams::default()
        };
        assert_eq!(workspace_root(&params).unwrap().as_str(), "file:///repo");
    }

    #[test]
    fn the_deprecated_root_uri_is_still_read() {
        // Deprecated in the spec, still the only thing some clients send.
        #[allow(deprecated)]
        let params = lsp_types::InitializeParams {
            root_uri: Some(lsp_types::Url::parse("file:///repo").unwrap()),
            ..lsp_types::InitializeParams::default()
        };
        assert_eq!(workspace_root(&params).unwrap().as_str(), "file:///repo");
        assert_eq!(workspace_root(&lsp_types::InitializeParams::default()), None);
    }
}
