//! Persistent undo history.
//!
//! A document's undo history is written next to no one — it lives in its own
//! directory, keyed by the hash of the document's canonical path, and is bound
//! to the contents it was saved with. On open the stored hash is compared
//! against the document; anything else (a `git checkout`, another editor, a
//! formatter) means the history no longer describes this file and is discarded.
//!
//! See `docs/superpowers/specs/2026-09-26-persistent-undo-design.md`.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use helix_core::history::{History, SerializedHistory};
use helix_core::Rope;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Version of the on-disk format. Bump this whenever the representation
/// changes: a history written by a different version is discarded rather than
/// parsed under rules it was not written for.
const FORMAT_VERSION: u32 = 1;

/// User-facing configuration for `[editor.persistent-undo]`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct PersistentUndoConfig {
    /// Whether to keep undo history across editing sessions. Defaults to `false`.
    pub enable: bool,
    /// Where to keep undo files. Defaults to `data_dir()/undo`.
    pub dir: Option<PathBuf>,
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

/// Reads the history stored for `path`, or `None` when there is nothing usable.
///
/// A missing file is the ordinary case for a document opened for the first time
/// and is not worth a warning; everything else is logged, because a history
/// that silently fails to come back is indistinguishable from one that was
/// never stored.
fn read(config: &PersistentUndoConfig, path: &Path, text: &Rope) -> Option<History> {
    let undo_file_path = undo_file(config, path);

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

    let undo_file: UndoFile = match serde_json::from_str(&contents) {
        Ok(undo_file) => undo_file,
        Err(err) => {
            log::warn!(
                "discarding unreadable undo history '{}': {err}",
                undo_file_path.display()
            );
            return None;
        }
    };

    if undo_file.version != FORMAT_VERSION {
        log::debug!(
            "discarding undo history '{}' written in format version {}",
            undo_file_path.display(),
            undo_file.version
        );
        return None;
    }

    if undo_file.content_sha256 != content_hash(text) {
        log::debug!(
            "discarding undo history for '{}': the file changed since it was stored",
            path.display()
        );
        return None;
    }

    match History::from_serialized(undo_file.history) {
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

#[cfg(test)]
mod tests {
    use super::*;

    use helix_core::history::State;
    use helix_core::{Selection, Transaction};

    fn config(dir: &Path) -> PersistentUndoConfig {
        PersistentUndoConfig {
            enable: true,
            dir: Some(dir.to_path_buf()),
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

    #[test]
    fn writes_then_reads_a_history_back() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let path = Path::new("/documents/hello.txt");
        let text = Rope::from("hello world\n");

        write(&config, path, &text, history().to_serialized(1));
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
            history().to_serialized(1),
        );

        assert!(read(&config, path, &Rope::from("something else\n")).is_none());
    }

    #[test]
    fn discards_a_history_written_by_another_format_version() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let path = Path::new("/documents/hello.txt");
        let text = Rope::from("hello world\n");

        write(&config, path, &text, history().to_serialized(1));

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
    fn discards_a_truncated_history() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let path = Path::new("/documents/hello.txt");
        let text = Rope::from("hello world\n");

        write(&config, path, &text, history().to_serialized(1));

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
            history().to_serialized(1),
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
        };
        write(
            &config,
            Path::new("/documents/hello.txt"),
            &Rope::from("hello world\n"),
            history().to_serialized(1),
        );
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
            history().to_serialized(1),
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
