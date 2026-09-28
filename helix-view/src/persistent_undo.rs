//! Persistent undo history.
//!
//! A document's undo history is not stored alongside the document; it lives in
//! its own directory, keyed by the hash of the document's canonical path, and
//! is bound to the contents it was saved with. On open the stored hash is compared
//! against the document; anything else (a `git checkout`, another editor, a
//! formatter) means the history no longer describes this file and is discarded.
//!
//! See `docs/superpowers/specs/2026-09-26-persistent-undo-design.md`.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use helix_core::history::{History, SerializedHistory};
use helix_core::Rope;
use helix_event::register_hook;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::events::{DocumentDidOpen, DocumentDidSave};
use crate::{DocumentId, Editor};

/// Version of the on-disk format. Bump this whenever the representation
/// changes: a history written by a different version is discarded rather than
/// parsed under rules it was not written for.
const FORMAT_VERSION: u32 = 1;

/// User-facing configuration for `[editor.persistent-undo]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct PersistentUndoConfig {
    /// Whether to keep undo history across editing sessions. Defaults to `false`.
    pub enable: bool,
    /// Where to keep undo files. Defaults to `data_dir()/undo`.
    pub dir: Option<PathBuf>,
    /// How much undo history to load into memory per document, in kibibytes.
    /// Defaults to 32768 (32 MiB).
    ///
    /// Older history beyond this is dropped at load time. There is no unlimited
    /// setting: unlimited is the behavior this budget exists to correct, since
    /// persistent undo otherwise reloads a document's entire accumulated
    /// history on every open.
    pub max_memory_kib: usize,
}

impl Default for PersistentUndoConfig {
    fn default() -> Self {
        Self {
            enable: false,
            dir: None,
            max_memory_kib: 32768,
        }
    }
}

/// The stored file. `current` and `revisions` are flattened in from
/// [`SerializedHistory`], so the file stays flat while helix-core keeps
/// ownership of the history's own representation.
#[derive(Debug, Serialize, Deserialize)]
struct UndoFile {
    version: u32,
    /// Kept for debugging the undo directory by hand; the file is keyed by a
    /// hash of this path, not by the path itself.
    path: PathBuf,
    content_sha256: String,
    #[serde(flatten)]
    history: SerializedHistory,
}

/// Just enough of [`UndoFile`] to check the format version before the rest of
/// the file is parsed. Unknown fields are ignored by default, so this
/// deserializes successfully against any version's file shape, including one
/// whose body is not a `SerializedHistory` at all.
#[derive(Debug, Deserialize)]
struct UndoFileVersion {
    version: u32,
}

fn undo_dir(config: &PersistentUndoConfig) -> PathBuf {
    match &config.dir {
        Some(dir) => helix_stdx::path::expand_tilde(dir).into_owned(),
        None => helix_loader::data_dir().join("undo"),
    }
}

/// The undo file for `path`, which is expected to be canonical.
///
/// Keyed by hash, like `helix_loader::workspace_trust`: documents with the same
/// name in different directories never collide, and no part of a path ever
/// becomes a file name.
fn undo_file(config: &PersistentUndoConfig, path: &Path) -> PathBuf {
    let mut hasher = Sha256::new();
    // `Path` is an `OsStr`; encode it lossily but deterministically, matching
    // what `workspace_trust` does for the same reason.
    hasher.update(path.as_os_str().to_string_lossy().as_bytes());
    undo_dir(config).join(format!("{}.json", hex_encode(&hasher.finalize())))
}

fn content_hash(text: &Rope) -> String {
    let mut hasher = Sha256::new();
    // Chunk boundaries are an implementation detail of the rope, but feeding
    // the chunks in order is the same as hashing the whole text.
    for chunk in text.chunks() {
        hasher.update(chunk.as_bytes());
    }
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The result of parsing an undo file's raw contents, distinguishing a version
/// mismatch from unreadable JSON so that `read`'s two "discarding" log
/// messages, and the tests that pin them, stay meaningful.
#[derive(Debug)]
enum ParsedUndoFile {
    /// Neither the version probe nor the full body could be parsed as JSON.
    Unreadable(serde_json::Error),
    /// The version probe parsed, but did not match [`FORMAT_VERSION`]. The
    /// full body is deliberately never parsed in this case: a different
    /// format version is free to change its shape entirely.
    VersionMismatch(u32),
    Ok(UndoFile),
}

/// Parses `contents` as an undo file, checking the format version before the
/// rest of the body so that a future format version with a different shape is
/// reported as a version mismatch rather than as unreadable JSON.
fn parse_undo_file(contents: &str) -> ParsedUndoFile {
    let version = match serde_json::from_str::<UndoFileVersion>(contents) {
        Ok(probe) => probe.version,
        Err(err) => return ParsedUndoFile::Unreadable(err),
    };

    if version != FORMAT_VERSION {
        return ParsedUndoFile::VersionMismatch(version);
    }

    match serde_json::from_str(contents) {
        Ok(undo_file) => ParsedUndoFile::Ok(undo_file),
        Err(err) => ParsedUndoFile::Unreadable(err),
    }
}

/// Reads the history stored for `path`, or `None` when there is nothing usable.
///
/// A missing file is the ordinary case for a document opened for the first time
/// and is not worth a warning; everything else is logged, because a history
/// that silently fails to come back is indistinguishable from one that was
/// never stored.
fn read(config: &PersistentUndoConfig, path: &Path, text: &Rope) -> Option<History> {
    let undo_file_path = undo_file(config, path);

    let max_bytes = config.max_memory_kib.saturating_mul(1024);

    // A file this large can only predate the budget or be corrupt. `serde_json`
    // materializes the whole document before the trim can run, so reading one
    // risks an out-of-memory at open; refusing is the safer failure.
    //
    // The threshold is measured against raw file bytes, not the payload the
    // trim itself measures (inserted and deleted text in the kept subtree). A
    // history of very many near-empty revisions can accumulate enough
    // structural overhead (indices, parent pointers, per-revision JSON
    // scaffolding) to cross this threshold while its actual payload would
    // have fit comfortably under budget once trimmed. That history is
    // discarded anyway: discarding is the safe failure for exactly the case
    // this guard exists for, and it self-heals, since the next save rewrites
    // the file from the in-memory history, bounded from then on.
    const FILE_SIZE_GUARD: u64 = 4;
    if let Ok(metadata) = fs::metadata(&undo_file_path) {
        if metadata.len() > (max_bytes as u64).saturating_mul(FILE_SIZE_GUARD) {
            log::warn!(
                "discarding undo history '{}': {} bytes exceeds {} times the {} KiB budget",
                undo_file_path.display(),
                metadata.len(),
                FILE_SIZE_GUARD,
                config.max_memory_kib
            );
            return None;
        }
    }

    let contents = match fs::read_to_string(&undo_file_path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        Err(err) => {
            log::warn!(
                "failed to read undo history '{}': {err}",
                undo_file_path.display()
            );
            return None;
        }
    };

    let undo_file = match parse_undo_file(&contents) {
        ParsedUndoFile::Ok(undo_file) => undo_file,
        ParsedUndoFile::VersionMismatch(version) => {
            log::warn!(
                "discarding undo history '{}' written in format version {version}",
                undo_file_path.display()
            );
            return None;
        }
        ParsedUndoFile::Unreadable(err) => {
            log::warn!(
                "discarding unreadable undo history '{}': {err}",
                undo_file_path.display()
            );
            return None;
        }
    };

    if undo_file.content_sha256 != content_hash(text) {
        log::debug!(
            "discarding undo history for '{}': the file changed since it was stored",
            path.display()
        );
        return None;
    }

    match History::from_serialized(undo_file.history, max_bytes) {
        Ok(history) => Some(history),
        Err(err) => {
            log::warn!(
                "discarding invalid undo history '{}': {err}",
                undo_file_path.display()
            );
            None
        }
    }
}

/// Stores `history` for `path`, bound to `text`.
///
/// Every failure is logged and swallowed: persisting undo history must never
/// interfere with saving the document itself.
fn write(config: &PersistentUndoConfig, path: &Path, text: &Rope, history: SerializedHistory) {
    let undo_file_path = undo_file(config, path);

    let contents = match serde_json::to_string(&UndoFile {
        version: FORMAT_VERSION,
        path: path.to_path_buf(),
        content_sha256: content_hash(text),
        history,
    }) {
        Ok(contents) => contents,
        Err(err) => {
            log::error!(
                "failed to serialize undo history for '{}': {err}",
                path.display()
            );
            return;
        }
    };

    if let Err(err) = write_atomically(&undo_file_path, &contents) {
        log::warn!(
            "failed to write undo history '{}': {err}",
            undo_file_path.display()
        );
    }
}

/// Writes through a temporary file in the same directory.
///
/// An interrupted write must not leave a truncated file behind: on the next
/// open that is indistinguishable from a history that was lost. `NamedTempFile`
/// creates the file with mode 0600 on unix and `persist` keeps those
/// permissions, which is what keeps deleted document text out of other users'
/// reach.
fn write_atomically(path: &Path, contents: &str) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "undo file path has no parent directory",
        )
    })?;
    fs::create_dir_all(dir)?;

    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(contents.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|err| err.error)?;

    Ok(())
}

/// Restores a document's history when it is opened.
fn restore(editor: &mut Editor, doc_id: DocumentId) {
    // Cloned out of the config guard so that the document can be borrowed
    // mutably below.
    let config = editor.config().persistent_undo.clone();
    if !config.enable {
        return;
    }

    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };
    let Some(path) = doc.path().map(PathBuf::from) else {
        return;
    };
    let Some(history) = read(&config, &path, doc.text()) else {
        return;
    };

    let current = history.current_revision();
    doc.history.set(history);

    // Only the revision needs updating here: `set_path` already picked up the
    // file's mtime into `last_saved_time` when the document was opened, with
    // the same `SystemTime::now()` fallback used below. It is re-read from the
    // file rather than reused because `last_saved_time` has no getter, not
    // because the external-modification guard in `Document::save_impl` would
    // otherwise break.
    let save_time = path
        .metadata()
        .and_then(|metadata| metadata.modified())
        .unwrap_or_else(|_| SystemTime::now());
    doc.set_last_saved_revision(current, save_time);
}

/// Stores a document's history after it has been written to disk.
///
/// `path` is the path that was actually written (see `DocumentDidSave`), not
/// `doc.path()`: on the `flush_writes` route (`:wq other.txt`, `:x
/// other.txt`, ...) `set_doc_path` is never called, so the document's own
/// path can still be stale here. `Document::save_impl` already canonicalizes
/// an explicit save-as path, and the document's own path is canonical too, so
/// this path is always the correct key as-is; no further canonicalization is
/// needed.
fn persist(editor: &mut Editor, doc_id: DocumentId, revision: usize, text: &Rope, path: &Path) {
    let config = editor.config().persistent_undo.clone();
    if !config.enable {
        return;
    }

    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };

    // The history lives in a `Cell` because parts of it are handed out by
    // reference elsewhere; take it out and put it straight back. This happens
    // before `revision` is used for anything, so a `revision` that turns out
    // to be out of range still leaves the document's in-memory history intact
    // rather than replaced by `History::default()`.
    let history = doc.history.take();
    let serialized = history.to_serialized(revision);
    doc.history.set(history);

    let Some(serialized) = serialized else {
        // `revision` is `DocumentSavedEvent::revision`, which was a valid
        // revision of this same history when the save started; it should
        // never be out of range here. But pairing the written text with an
        // arbitrary revision (or panicking) would be worse than skipping the
        // write, so this is a hard requirement rather than a debug assertion.
        log::warn!(
            "not persisting undo history for '{}': revision {revision} is not a valid revision \
             of the document's history",
            path.display()
        );
        return;
    };

    write(&config, path, text, serialized);
}

pub fn register_hooks() {
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        restore(event.editor, event.doc);
        Ok(())
    });

    register_hook!(move |event: &mut DocumentDidSave<'_>| {
        persist(
            event.editor,
            event.doc,
            event.revision,
            event.text,
            event.path,
        );
        Ok(())
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use helix_core::history::State;
    use helix_core::{Selection, Transaction};

    fn config(dir: &Path) -> PersistentUndoConfig {
        PersistentUndoConfig {
            enable: true,
            dir: Some(dir.to_path_buf()),
            ..PersistentUndoConfig::default()
        }
    }

    /// A history holding a single edit of "hello\n" into "hello world\n".
    fn history() -> History {
        let mut state = State {
            doc: Rope::from("hello\n"),
            selection: Selection::point(0),
        };
        let mut history = History::default();
        let transaction =
            Transaction::change(&state.doc, [(5, 5, Some(" world".into()))].into_iter());
        history.commit_revision(&transaction, &state);
        transaction.apply(&mut state.doc);
        history
    }

    /// A history holding a single edit that inserts a large enough payload
    /// for its serialized form to exceed the file-size guard's threshold at a
    /// small budget, while still being ordinary, valid, parseable JSON.
    fn large_history() -> History {
        let mut state = State {
            doc: Rope::from("hello\n"),
            selection: Selection::point(0),
        };
        let mut history = History::default();
        let big_insert = "x".repeat(8 * 1024);
        let transaction =
            Transaction::change(&state.doc, [(5, 5, Some(big_insert.into()))].into_iter());
        history.commit_revision(&transaction, &state);
        transaction.apply(&mut state.doc);
        history
    }

    #[test]
    fn writes_then_reads_a_history_back() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let path = Path::new("/documents/hello.txt");
        let text = Rope::from("hello world\n");

        write(&config, path, &text, history().to_serialized(1).unwrap());
        let restored = read(&config, path, &text).expect("history should be restored");

        assert_eq!(restored.current_revision(), 1);
    }

    #[test]
    fn returns_nothing_when_no_history_was_stored() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(
            &config(dir.path()),
            Path::new("/documents/absent.txt"),
            &Rope::from("hello\n")
        )
        .is_none());
    }

    #[test]
    fn discards_a_history_whose_document_changed_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let path = Path::new("/documents/hello.txt");

        write(
            &config,
            path,
            &Rope::from("hello world\n"),
            history().to_serialized(1).unwrap(),
        );

        assert!(read(&config, path, &Rope::from("something else\n")).is_none());
    }

    #[test]
    fn discards_a_history_written_by_another_format_version() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let path = Path::new("/documents/hello.txt");
        let text = Rope::from("hello world\n");

        write(&config, path, &text, history().to_serialized(1).unwrap());

        let stored = undo_file(&config, path);
        let contents = fs::read_to_string(&stored).unwrap();
        fs::write(
            &stored,
            contents.replace(r#""version":1"#, r#""version":2"#),
        )
        .unwrap();

        assert!(read(&config, path, &text).is_none());
    }

    #[test]
    fn a_version_mismatch_is_detected_before_the_body_is_parsed() {
        // A hypothetical format version 2 with a completely different body
        // shape must still be recognized as a version mismatch, not reported
        // as unreadable JSON: `version` is checked against a probe struct
        // before the rest of the file is parsed as a `UndoFile`.
        let contents = r#"{"version":2,"totally":"different","shape":[1,2,3]}"#;
        assert!(matches!(
            parse_undo_file(contents),
            ParsedUndoFile::VersionMismatch(2)
        ));
    }

    #[test]
    fn unparsable_json_is_reported_as_unreadable() {
        assert!(matches!(
            parse_undo_file("not json"),
            ParsedUndoFile::Unreadable(_)
        ));
    }

    #[test]
    fn discards_a_truncated_history() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let path = Path::new("/documents/hello.txt");
        let text = Rope::from("hello world\n");

        write(&config, path, &text, history().to_serialized(1).unwrap());

        let stored = undo_file(&config, path);
        let contents = fs::read_to_string(&stored).unwrap();
        fs::write(&stored, &contents[..contents.len() / 2]).unwrap();

        assert!(read(&config, path, &text).is_none());
    }

    #[test]
    fn keys_undo_files_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());

        assert_eq!(
            undo_file(&config, Path::new("/a/notes.txt")),
            undo_file(&config, Path::new("/a/notes.txt"))
        );
        assert_ne!(
            undo_file(&config, Path::new("/a/notes.txt")),
            undo_file(&config, Path::new("/b/notes.txt"))
        );
    }

    #[test]
    fn leaves_no_temporary_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let path = Path::new("/documents/hello.txt");

        write(
            &config,
            path,
            &Rope::from("hello world\n"),
            history().to_serialized(1).unwrap(),
        );

        let entries: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn survives_an_unwritable_undo_directory() {
        // A plain file where the undo directory should be: `create_dir_all`
        // fails for every user, including root. Saving a document must not be
        // affected by this.
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("undo");
        fs::write(&blocked, "not a directory").unwrap();

        let config = PersistentUndoConfig {
            enable: true,
            dir: Some(blocked),
            ..PersistentUndoConfig::default()
        };
        write(
            &config,
            Path::new("/documents/hello.txt"),
            &Rope::from("hello world\n"),
            history().to_serialized(1).unwrap(),
        );
    }

    #[test]
    fn the_default_budget_is_32_mib() {
        let config = PersistentUndoConfig::default();
        assert_eq!(config.max_memory_kib, 32768);
        assert!(!config.enable);
    }

    #[test]
    fn a_config_without_the_budget_key_still_deserializes() {
        // Configs written before this setting existed must keep working and
        // pick up the default, rather than failing to parse.
        let config: PersistentUndoConfig = toml::from_str("enable = true\n").unwrap();
        assert!(config.enable);
        assert_eq!(config.max_memory_kib, 32768);
    }

    #[test]
    fn an_oversized_undo_file_is_discarded_unread() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(dir.path());
        // 1 KiB of budget allows at most 4 KiB of file.
        config.max_memory_kib = 1;
        let path = Path::new("/documents/hello.txt");
        let text = Rope::from("hello world\n");

        // Written through the ordinary `write` path, so the file is valid,
        // parseable JSON that would deserialize successfully if read: only
        // the guard can explain `read` returning `None` below, not a parse
        // failure. `large_history` inserts enough text that the file lands
        // comfortably past the 4 KiB threshold.
        write(
            &config,
            path,
            &text,
            large_history().to_serialized(1).unwrap(),
        );
        let stored = undo_file(&config, path);
        let size = fs::metadata(&stored).unwrap().len();
        assert!(
            size > 4 * 1024,
            "test file must exceed the 4 KiB guard to exercise it: was {size} bytes"
        );

        assert!(read(&config, path, &text).is_none());
    }

    #[test]
    fn a_file_within_the_guard_is_still_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(dir.path());
        config.max_memory_kib = 1;
        let path = Path::new("/documents/hello.txt");
        let text = Rope::from("hello world\n");

        write(&config, path, &text, history().to_serialized(1).unwrap());
        // The written file is far below 4 KiB, so the guard must not fire.
        assert!(std::fs::metadata(undo_file(&config, path)).unwrap().len() < 4 * 1024);
        assert!(read(&config, path, &text).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn stores_undo_files_readable_only_by_their_owner() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let path = Path::new("/documents/hello.txt");

        write(
            &config,
            path,
            &Rope::from("hello world\n"),
            history().to_serialized(1).unwrap(),
        );

        // Undo files hold text deleted from the document, including text the
        // document itself no longer contains.
        let mode = fs::metadata(undo_file(&config, path))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
