# Persistent undo

Date: 2026-09-26
Status: approved, ready for implementation planning

## Goal

Persist a document's undo/redo history across editor sessions, like vim's `undofile`.

Two requirements carry as much weight as the functionality itself:

1. **The patch must survive upstream updates.** This fork regularly pulls new
   helix releases. The implementation must minimise its contact surface with
   upstream files and avoid hunks inside functions that change often.
2. **The implementation must sit on the editor's existing architecture** rather
   than beside it.

The starting point is a working draft:
<https://gist.github.com/aliev/02b6ec9dde28c85298af17f8ccebff11>. It is
functionally correct; this spec moves its logic onto helix's own mechanisms and
fixes several defects along the way (see "Differences from the draft").

## Out of scope

- Persisting unsaved buffer contents (that is a swap file, a different feature).
- Rebasing history onto a file that changed outside the editor.
- Bounding history size or garbage-collecting the undo directory.
- Commands such as `:undo-clean`.

## Key decisions

### Strict content binding

The undo file stores a `sha256` of the document contents at write time. On open
the hash is compared against the current contents; a mismatch means the file
changed outside helix (git checkout, another editor, a formatter), and the
history is **silently discarded** with a note in `helix.log`. This matches vim's
`undofile` behaviour. Applying a foreign history to a buffer is impossible by
construction.

### Field privacy as a patch-isolation tool

Rust visibility rule: a type's private fields are reachable from the module that
declares the type **and all of its descendants**. The layout `src/transaction.rs`
plus `src/transaction/persist.rs` makes `persist` a child of `transaction`, which
grants access to the private `ChangeSet::len`, `ChangeSet::len_after`,
`ChangeSet::changes` and to `Transaction`'s fields.

Consequently serialization needs neither a new public API in helix-core (the
draft added `Transaction::len_chars`) nor lossy mirror structs. The cost is
**one line** — `mod persist;` — in an upstream file.

### Helix's own mechanisms instead of edits to hot functions

- Restoring history hooks the existing `DocumentDidOpen` event.
- Writing history hooks a new `DocumentDidSave` event, dispatched at the end of
  `Application::handle_document_write`.

`Document::open` and `Document::save_impl` are not touched at all. That is the
main win for requirement 1: both functions see regular upstream churn (atomic
save, symlink/hardlink handling, editorconfig — all within the last year), and
the draft placed roughly 80 lines inside them.

Adding `DocumentDidSave` is also a self-contained candidate for an upstream PR:
helix already has `DocumentDidOpen`, `DocumentDidChange`, `DocumentDidClose` and
`DocumentFocusLost`, so the missing save event is a gap rather than a deliberate
omission.

## Architecture

### New files (fork-owned, never a rebase conflict)

| File | Responsibility |
|---|---|
| `helix-core/src/transaction/persist.rs` | serde representation of `Operation`, `ChangeSet`, `Transaction` via `#[serde(remote)]`. Invariant validation on deserialization. `Tendril` serialized as a string. |
| `helix-core/src/history/persist.rs` | `SerializedHistory` — flat data (revisions, `current`, absolute timestamps). `History::to_serialized` / `History::from_serialized` with revision-graph validation. `Instant` ↔ `SystemTime` conversion. No I/O. |
| `helix-view/src/persistent_undo.rs` | `PersistentUndoConfig`, undo-file path derivation, the on-disk envelope, atomic writes, both hooks, `register_hooks()`. |
| `helix-term/tests/test/persistent_undo.rs` | End-to-end test driving a real `Application`. |

The boundary: **helix-core knows how to turn a `History` into flat data and
nothing about files; helix-view knows where the file lives and when to touch it
and nothing about `History` internals.** Both halves are tested independently.

### Upstream edits (~25 lines, every hunk append-shaped)

```
helix-core/src/transaction.rs      +1   mod persist;
helix-core/src/history.rs          +2   mod persist;  pub use persist::SerializedHistory;
helix-view/src/lib.rs              +1   pub mod persistent_undo;
helix-view/Cargo.toml              +1   sha2 = "0.11"   (already in the tree via helix-loader)
helix-view/src/editor.rs           +3   use + Config field + Default line
helix-view/src/events.rs           +7   DocumentDidSave appended to the events!{} block
helix-view/src/handlers.rs         +1   persistent_undo::register_hooks();
helix-term/src/events.rs           +2   import + register_event::<DocumentDidSave>();
helix-term/src/application.rs      +6   dispatch at the end of handle_document_write
helix-term/tests/integration.rs    +1   mod persistent_undo;
book/src/editor.md                 +~15 documentation
```

For comparison, the draft is 377 lines across 4 files, ~80 of them inside
`Document::save_impl` and `Document::open`.

### The new event

```rust
// helix-view/src/events.rs, appended to the events!{} block
DocumentDidSave<'a> {
    editor: &'a mut Editor,
    doc: DocumentId,
    revision: usize,
    text: &'a Rope
}
```

Dispatched at the end of `Application::handle_document_write`, **after**
`Editor::set_doc_path`, so that `:w other.txt` writes history under the new path.

### Configuration

A nested section in the style of `[editor.soft-wrap]` and `[editor.smart-tab]`:

```toml
[editor.persistent-undo]
enable = true
# dir = "~/.local/share/helix/undo"   # defaults to data_dir()/undo
```

`PersistentUndoConfig { enable: bool, dir: Option<PathBuf> }` is declared in
`helix-view/src/persistent_undo.rs`; `editor.rs` only gains the field and its
`Default` line. The default is `enable = false`.

A configurable `dir` is more than a convenience: it lets tests use
`tempfile::tempdir()` instead of overriding `HOME`/`XDG_DATA_HOME` through
`std::env::set_var` the way the draft does, which is unsound in a multi-threaded
test runner and breaks neighbouring tests.

## On-disk format

Path: `<dir>/<sha256(canonical_path)>.json`.

The scheme mirrors the existing precedent in
`helix-loader/src/workspace_trust.rs` (`data_dir()/workspace_trust/<sha256(path)>`),
including storing the original path inside the file for debugging.

```json
{
  "version": 1,
  "path": "/home/ali/Projects/helix/foo.rs",
  "content_sha256": "3f786850e387550f…",
  "current": 42,
  "revisions": [
    {
      "parent": 41,
      "last_child": null,
      "transaction": {
        "changes": { "changes": [{"Retain": 5}, {"Insert": " world"}],
                     "len": 5, "len_after": 11 },
        "selection": { "ranges": [{"anchor": 11, "head": 11, "old_visual_position": null}],
                       "primary_index": 0 }
      },
      "inversion": { "…": "…" },
      "timestamp_unix_ms": 1758880000000
    }
  ]
}
```

The envelope (`version`, `path`, `content_sha256`) is declared in helix-view,
while `current` and `revisions` come from helix-core's `SerializedHistory` via
`#[serde(flatten)]`: the file stays flat while the crate boundary holds.

A `version` mismatch discards the history. The file is not deleted — it will be
overwritten on the next save.

### Serializing transactions

`#[serde(remote = "ChangeSet")]`, `#[serde(remote = "Transaction")]`,
`#[serde(remote = "Operation")]`, `#[serde(remote = "Range")]`.

Besides exactness (no reconstruction through `Transaction::change` and no dummy
rope), remote derive **checks the field set against the original at compile
time**. If upstream adds a field to `ChangeSet` or `Transaction`, updating the
fork fails to build — instead of silently dropping data, which is what a
hand-written mirror would do.

`Selection` uses a dedicated `SerializedSelection` struct rather than remote
derive, because restoration has to be able to fail: `Selection::new` panics on an
empty range vector, so an empty list from a corrupted file must be rejected
before construction.

`Tendril` (`smartstring::SmartString`) is serialized as a string through a local
`mod tendril_serde`, which avoids enabling the `smartstring/serde` feature.
`helix-core/Cargo.toml` is left untouched.

### The current pointer

The file stores `DocumentSavedEvent::revision` — the revision matching the text
that actually reached the disk — not `history.current` at the time the event is
handled.

Saving is asynchronous, so by the time `DocumentDidSave` arrives the document may
have moved on. Revisions created after the write started are still serialized
(they are valid and reachable through redo), but the pointer refers to the
on-disk state. In that situation the draft stored the hash of text A together
with a pointer to state B, and the next open discarded the history on hash
mismatch.

### Timestamps

Stored as absolute `unix_ms`. On write, `Instant` is converted to `SystemTime`
through a single reference pair `(Instant::now(), SystemTime::now())`; on read it
is converted back **into the past**: `rev.instant = now - (now_sys - rev.sys)`.

On Linux and macOS `Instant` counts from boot, so
`Instant::now().checked_sub(7 days)` returns `None` when uptime is two days — the
common case. The largest representable offset is probed once (a decreasing
sequence 30d → 7d → 1d → 1h → 1m → 0) and every revision older than that collapses
onto that earliest point.

Consequence: after a reboot, `:earlier 2d` jumps to the start of the restored
history rather than to an exact position. That is predictable, unlike the draft,
which placed restored revisions in the **future** (`now + delta`); new commits
stamped with `Instant::now()` then sorted before older ones and broke the
monotonicity `:earlier` and `:later` rely on.

## Flow

### Restore — `DocumentDidOpen` hook

1. Is `config.persistent_undo.enable` set? Otherwise return.
2. Does the document have a `path()`? Otherwise return (scratch buffer).
3. Read the file (blocking; it is small). `NotFound` returns silently.
4. `version == 1`? Otherwise `log::warn!` and return.
5. `content_sha256 == sha256(doc.text())`? Otherwise `log::debug!` and return.
6. `History::from_serialized` with validation. On error, `log::warn!` and return.
7. `doc.history.set(history)` and `doc.set_last_saved_revision(current, mtime)` —
   without the second call the document would look modified right after opening.
   `mtime` is read from the file's metadata so that the external-modification
   guard in `save_impl` ("file modified by an external process") keeps working.

No failure path modifies the document.

### Write — `DocumentDidSave` hook

1. Enabled? Has a `path()`? Otherwise return.
2. `hash = sha256(event.text)`.
3. `let history = doc.history.take(); let data = history.to_serialized(event.revision); doc.history.set(history);`
4. Serialize the envelope to a `String` on the current thread (kilobytes).
5. `tokio::spawn`: `create_dir_all`, write a temporary file in the same
   directory, `0600` on unix, `rename` over the target.

Errors only go to the log; they never affect saving the document.

Atomicity via `rename` is not cosmetic: without it a crash or `kill` during the
write leaves truncated JSON, which on the next open looks like lost history.

## Edge cases

| Case | Behaviour |
|---|---|
| Two helix instances on one file | `rename` is atomic; last writer wins; corruption is impossible |
| Symlink / hardlink | The key derives from `canonicalize()`, as in `Editor::open`, so one physical file maps to one undo file |
| `:w other.txt` (save-as) | Dispatch happens after `set_doc_path`, so history is written under the new path; the old path's undo file stays valid |
| Scratch buffer | No path — ignored |
| `:reload` | The undo file is left alone until the next `:w` |
| Corrupted or truncated file | Validation refuses to construct an inconsistent `ChangeSet`, so a broken history never reaches `Transaction::apply`. Worst case is a discarded history |
| Privacy | The undo file contains deleted text in the clear. `0600` on unix plus a warning in the documentation |
| Directory growth | Unbounded, as in vim. Expect 1–2 KB per editing session |

## Testing

TDD, bottom-up, layer by layer.

**1. `helix-core/src/transaction/persist.rs`**
- Round-trip for `Retain`, `Delete` and `Insert`.
- Rejects `len`/`len_after` inconsistent with the operation list.
- Rejects syntactic garbage.
- For a set of transactions, applying the round-tripped transaction yields the
  same result as applying the original.

**2. `helix-core/src/history/persist.rs`**
- Round-trip of a **branching** history: edit → undo → different edit creates a
  branch; `last_child` is preserved and redo follows the latest branch.
- Rejects `parent >= index`, `current >= len`, an empty revision vector and an
  out-of-range `last_child`.
- Restored timestamps stay monotonic with respect to subsequent commits.

**3. `helix-view/src/persistent_undo.rs`**
- Path derivation is stable and depends only on the canonical path.
- A version mismatch is discarded.
- A hash mismatch is discarded.
- No temporary files remain in the directory after a write.

Every test uses `tempfile::tempdir()` and an explicit `dir` in the config.

**4. `helix-term/tests/test/persistent_undo.rs`**
- Full cycle: open → edit → `:w` → new session → `u` restores the original
  contents.
- Negative: the file changes on disk between sessions → `u` does nothing.

## Documentation

A section in `book/src/editor.md` alongside the other nested config sections:
what it does, a config example, where the files live, what happens when the file
changes outside the editor, and a warning that undo files contain deleted text in
the clear.

## Differences from the draft

| Draft | Here | Why |
|---|---|---|
| `DefaultHasher` for both the file key and the content hash | `sha256` | `DefaultHasher` is explicitly not stable across Rust versions: after a toolchain upgrade every undo file silently detaches from its document |
| `Rope::from(" ".repeat(len))` per revision on restore | Direct `ChangeSet` reconstruction | On a 10 MB file with 500 revisions the draft performs gigabytes of memcpy at open time |
| Restored timestamps in the future (`now + delta`) | Absolute `unix_ms`, restored into the past with clamping | Otherwise new commits sort before old ones and break `:earlier` / `:later` |
| `current` taken from the history when the event is handled | `DocumentSavedEvent::revision` | Fixes the race between the asynchronous write and further edits |
| Hand-written mirror structs | `#[serde(remote)]` | An upstream field addition fails the build instead of being silently dropped |
| No format version field | `version: 1` | A format change between fork versions must not lead to parsing an old file under new rules |
| Non-atomic write | Temporary file + `rename`, `0600` | A truncated file looks like lost history, and the undo file contains document text |
| `std::env::set_var("HOME")` in tests | Explicit `dir` in the config | Overriding environment variables is unsound in a multi-threaded test runner and breaks neighbouring tests |
| Logic inside `Document::open` and `save_impl` | Event hooks | Both functions see regular upstream churn |
