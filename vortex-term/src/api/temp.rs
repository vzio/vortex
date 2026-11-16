//! Temporary documents: opened on a language server with `didOpen`, never written to disk, and
//! closed with `didClose` when dropped (a request done, its task aborted, the browser closed).
//! While open, and [`HiddenDocuments::LINGER`] after, the diagnostics the server sends for them
//! are dropped (`Application`'s `publishDiagnostics`).
//!
//! The browser opens two kinds: probes from the templates of `api-probe` (under a virtual path
//! no file has) and real files the editor doesn't have open (with their text on disk), for the
//! servers that answer only about open documents. A file the editor has open is never opened
//! again: its buffer is the user's.

use std::sync::Arc;

use vortex_lsp::{lsp, Client, HiddenDocuments};

pub(crate) struct TempDocument {
    client: Arc<Client>,
    hidden: HiddenDocuments,
    uri: lsp::Url,
}

impl TempDocument {
    pub fn open(
        client: &Arc<Client>,
        hidden: &HiddenDocuments,
        uri: lsp::Url,
        language_id: String,
        text: &str,
    ) -> Self {
        // Listed first, so that diagnostics sent right away are dropped. Opened on the server by
        // the first of the tasks that need it at the same time, closed by the last.
        if !hidden.open(uri.clone()) {
            return Self {
                client: client.clone(),
                hidden: hidden.clone(),
                uri,
            };
        }
        client.notify::<lsp::notification::DidOpenTextDocument>(lsp::DidOpenTextDocumentParams {
            text_document: lsp::TextDocumentItem {
                uri: uri.clone(),
                language_id,
                version: 0,
                text: text.to_string(),
            },
        });
        Self {
            client: client.clone(),
            hidden: hidden.clone(),
            uri,
        }
    }
}

impl Drop for TempDocument {
    fn drop(&mut self) {
        if self.hidden.close(&self.uri) {
            self.client
                .text_document_did_close(lsp::TextDocumentIdentifier {
                    uri: self.uri.clone(),
                });
        }
    }
}
