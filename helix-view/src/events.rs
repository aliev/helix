use std::path::Path;

use helix_core::{ChangeSet, Rope};
use helix_event::events;
use helix_lsp::LanguageServerId;

use crate::{editor::Config, Document, DocumentId, Editor, ViewId};

events! {
    DocumentDidOpen<'a> {
        editor: &'a mut Editor,
        doc: DocumentId
    }
    DocumentDidChange<'a> {
        doc: &'a mut Document,
        view: ViewId,
        old_text: &'a Rope,
        changes: &'a ChangeSet,
        ghost_transaction: bool
    }
    DocumentDidClose<'a> {
        editor: &'a mut Editor,
        doc: Document
    }
    SelectionDidChange<'a> { doc: &'a mut Document, view: ViewId }
    DiagnosticsDidChange<'a> { editor: &'a mut Editor, doc: DocumentId }
    // called **after** a document loses focus (but not when its closed)
    DocumentFocusLost<'a> { editor: &'a mut Editor, doc: DocumentId }

    // called **after** a document's contents have reached the disk
    DocumentDidSave<'a> {
        editor: &'a mut Editor,
        doc: DocumentId,
        // The revision whose contents were written.
        revision: usize,
        // The text that was written, which is not necessarily the document's
        // current text: saving is asynchronous.
        text: &'a Rope,
        // The path that was actually written. Not always the document's own
        // `path()`: `flush_writes` never calls `set_doc_path`, so on a
        // save-as-and-quit (`:wq other.txt`, `:x other.txt`, ...) the
        // document's path is still stale when this event fires.
        path: &'a Path
    }

    LanguageServerInitialized<'a> {
        editor: &'a mut Editor,
        server_id: LanguageServerId
    }
    LanguageServerExited<'a> {
        editor: &'a mut Editor,
        server_id: LanguageServerId
    }

    // NOTE: this event is simple for now and is expected to change as the config system evolves.
    // Ideally it would say what changed.
    ConfigDidChange<'a> {
        editor: &'a mut Editor,
        old: &'a Config,
        new: &'a Config
    }
}
