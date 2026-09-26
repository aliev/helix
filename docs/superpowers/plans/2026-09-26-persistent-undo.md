# Persistent Undo Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist a document's undo/redo history across editor sessions, so that reopening a file and pressing `u` walks back through edits made in an earlier session.

**Architecture:** Serialization lives in helix-core as two child modules (`transaction::persist`, `history::persist`) that reach the private fields of `ChangeSet`, `Transaction`, `History` and `Revision` through Rust's "children see their ancestors' private items" rule. File layout, hashing and timing live in a single helix-view module driven by two event hooks: the existing `DocumentDidOpen` and a new `DocumentDidSave`. `Document::open` and `Document::save_impl` are never modified, which is what keeps the patch rebaseable.

**Tech Stack:** Rust, serde + serde_json (already in helix-core and helix-view), sha2 (already in the tree via helix-loader), tempfile (already a helix-view dependency), helix-event hooks.

**Spec:** `docs/superpowers/specs/2026-09-26-persistent-undo-design.md`

## Global Constraints

- The feature is off by default: `PersistentUndoConfig::default()` is `enable: false, dir: None`.
- On-disk format version is `1`. Any other value causes the history to be discarded, never parsed.
- Undo files live at `<dir>/<sha256(canonical path)>.json`, where `<dir>` defaults to `helix_loader::data_dir().join("undo")`.
- No changes to `helix-core/Cargo.toml`. The only dependency added anywhere is `sha2 = "0.11"` in `helix-view/Cargo.toml`.
- `Document::open` and `Document::save_impl` must not be modified.
- Every failure path — missing file, unreadable file, wrong version, hash mismatch, invalid history, unwritable directory — logs and returns. None of them may modify the document, fail a save, or panic.
- Mirror types convert through struct literals and exhaustive `match`es so that a field or variant added upstream is a compile error.
- No test may call `std::env::set_var`. Tests point `dir` at a `tempfile::tempdir()`.

## Review Focus

1. The undo directory cannot be created or written (read-only volume, a plain file in its place): `:w` must still save the document and report success.
2. `:x` / `:wq` / `:wqa`: these drain the save queue through `Editor::flush_writes`, not `Application::handle_document_write`; history must be persisted on this path too.
3. `:w other.txt`: the dispatch happens after `Editor::set_doc_path`, so the history must be keyed to the new path.
4. Editing further in a restored session: `u` must walk from the new edit back into the restored history, not stop at the session boundary.
5. The file changed on disk between sessions: the history is discarded and `u` does nothing to the buffer.

---

### Task 1: Transaction serialization in helix-core

**Files:**
- Create: `helix-core/src/transaction/persist.rs`
- Modify: `helix-core/src/transaction.rs` (one line, after the `use` block at the top)
- Test: `helix-core/src/transaction/persist.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub(crate) struct SerializedTransaction` with `impl From<&Transaction> for SerializedTransaction` and `pub(crate) fn into_transaction(self) -> Result<Transaction, InvalidHistory>`.
  - `InvalidHistory` is defined in Task 2 and imported here as `crate::history::InvalidHistory`. Task 1 and Task 2 therefore only compile together — implement Task 2's error type first if you are building them in one pass, or accept that `cargo check` fails until Task 2 lands.

- [ ] **Step 1: Add the error type that both tasks share**

This is the one piece of Task 2 that Task 1 needs. Create `helix-core/src/history/persist.rs` with only this content for now:

```rust
//! Persistence of undo history. See `docs/superpowers/specs/2026-09-26-persistent-undo-design.md`.

/// Returned when a persisted history cannot be trusted and must be discarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidHistory(&'static str);

impl InvalidHistory {
    pub(crate) fn new(reason: &'static str) -> Self {
        Self(reason)
    }
}

impl std::fmt::Display for InvalidHistory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for InvalidHistory {}
```

Add to `helix-core/src/history.rs`, directly below the existing `use` statements at the top of the file:

```rust
mod persist;

pub use persist::InvalidHistory;
```

- [ ] **Step 2: Write the failing test**

Create `helix-core/src/transaction/persist.rs` containing only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Rope, Selection};

    fn roundtrip(transaction: &Transaction) -> Transaction {
        let json = serde_json::to_string(&SerializedTransaction::from(transaction)).unwrap();
        serde_json::from_str::<SerializedTransaction>(&json)
            .unwrap()
            .into_transaction()
            .unwrap()
    }

    #[test]
    fn roundtrips_retain_insert_and_delete() {
        let doc = Rope::from("hello world\n");
        // Retain 5, insert ", cruel", retain 6, delete the newline.
        let transaction = Transaction::change(
            &doc,
            [(5, 5, Some(", cruel".into())), (11, 12, None)].into_iter(),
        )
        .with_selection(Selection::single(0, 3));

        let restored = roundtrip(&transaction);
        assert_eq!(transaction, restored);

        let mut expected = doc.clone();
        assert!(transaction.apply(&mut expected));
        let mut actual = doc.clone();
        assert!(restored.apply(&mut actual));
        assert_eq!(expected, actual);
    }

    #[test]
    fn roundtrips_a_transaction_without_a_selection() {
        let doc = Rope::from("abc");
        let transaction = Transaction::change(&doc, [(0, 1, None)].into_iter());
        assert_eq!(transaction, roundtrip(&transaction));
    }

    #[test]
    fn rejects_a_change_set_whose_length_disagrees_with_its_operations() {
        // `len` claims the original document was 99 characters, the operations
        // account for 5. Trusting it would make `Transaction::apply` refuse the
        // change or, worse, operate on the wrong document.
        let json = r#"{"changes":{"changes":[{"Retain":5}],"len":99,"len_after":5},"selection":null}"#;
        let error = serde_json::from_str::<SerializedTransaction>(json)
            .unwrap()
            .into_transaction()
            .unwrap_err();
        assert!(error.to_string().contains("change set length"));
    }

    #[test]
    fn rejects_a_selection_without_ranges() {
        // `Selection::new` panics on an empty range list.
        let json = r#"{"changes":{"changes":[],"len":0,"len_after":0},"selection":{"ranges":[],"primary_index":0}}"#;
        let error = serde_json::from_str::<SerializedTransaction>(json)
            .unwrap()
            .into_transaction()
            .unwrap_err();
        assert!(error.to_string().contains("no ranges"));
    }

    #[test]
    fn rejects_a_selection_whose_primary_index_is_out_of_bounds() {
        let json = r#"{"changes":{"changes":[],"len":0,"len_after":0},"selection":{"ranges":[{"anchor":0,"head":0,"old_visual_position":null}],"primary_index":7}}"#;
        let error = serde_json::from_str::<SerializedTransaction>(json)
            .unwrap()
            .into_transaction()
            .unwrap_err();
        assert!(error.to_string().contains("primary index"));
    }
}
```

Add to `helix-core/src/transaction.rs`, directly below the existing `use` statements at the top of the file:

```rust
pub(crate) mod persist;
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p helix-core --lib transaction::persist`
Expected: FAIL — compile errors, `cannot find type SerializedTransaction in this scope`.

- [ ] **Step 4: Write the implementation**

Prepend to `helix-core/src/transaction/persist.rs`, above the test module:

```rust
//! Serialization of [`Transaction`]s for persistent undo history.
//!
//! This module is a child of `transaction`, which is what gives it access to the
//! private fields of [`ChangeSet`] and [`Transaction`]. The conversions below
//! deliberately use struct literals and exhaustive matches: if a field or a
//! variant is added upstream, this module stops compiling instead of silently
//! dropping data.

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use super::{ChangeSet, Operation, Transaction};
use crate::history::InvalidHistory;
use crate::{Range, Selection};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SerializedTransaction {
    changes: SerializedChangeSet,
    selection: Option<SerializedSelection>,
}

impl From<&Transaction> for SerializedTransaction {
    fn from(transaction: &Transaction) -> Self {
        Self {
            changes: SerializedChangeSet::from(&transaction.changes),
            selection: transaction.selection.as_ref().map(SerializedSelection::from),
        }
    }
}

impl SerializedTransaction {
    pub(crate) fn into_transaction(self) -> Result<Transaction, InvalidHistory> {
        let selection = self
            .selection
            .map(SerializedSelection::into_selection)
            .transpose()?;

        Ok(Transaction {
            changes: self.changes.into_change_set()?,
            selection,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializedChangeSet {
    changes: Vec<SerializedOperation>,
    len: usize,
    len_after: usize,
}

impl From<&ChangeSet> for SerializedChangeSet {
    fn from(changes: &ChangeSet) -> Self {
        Self {
            changes: changes
                .changes
                .iter()
                .map(SerializedOperation::from)
                .collect(),
            len: changes.len,
            len_after: changes.len_after,
        }
    }
}

impl SerializedChangeSet {
    fn into_change_set(self) -> Result<ChangeSet, InvalidHistory> {
        let changes: Vec<Operation> = self.changes.into_iter().map(Operation::from).collect();

        // Recompute both lengths rather than trusting the file. `ChangeSet::len`
        // is the document length the change set may be applied to, so a corrupted
        // value either makes every later `apply` fail or lets one run against a
        // document it was not built for.
        let mut len = 0;
        let mut len_after = 0;
        for operation in &changes {
            match operation {
                Operation::Retain(n) => {
                    len += n;
                    len_after += n;
                }
                Operation::Delete(n) => len += n,
                Operation::Insert(text) => len_after += text.chars().count(),
            }
        }

        if len != self.len || len_after != self.len_after {
            return Err(InvalidHistory::new(
                "change set length disagrees with its operations",
            ));
        }

        // A struct literal, so that a field added to `ChangeSet` upstream is a
        // compile error here instead of a silently dropped value.
        Ok(ChangeSet {
            changes,
            len: self.len,
            len_after: self.len_after,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
enum SerializedOperation {
    Retain(usize),
    Delete(usize),
    Insert(String),
}

impl From<&Operation> for SerializedOperation {
    fn from(operation: &Operation) -> Self {
        // Exhaustive: a variant added upstream breaks this match.
        match operation {
            Operation::Retain(n) => Self::Retain(*n),
            Operation::Delete(n) => Self::Delete(*n),
            Operation::Insert(text) => Self::Insert(text.to_string()),
        }
    }
}

impl From<SerializedOperation> for Operation {
    fn from(operation: SerializedOperation) -> Self {
        match operation {
            SerializedOperation::Retain(n) => Operation::Retain(n),
            SerializedOperation::Delete(n) => Operation::Delete(n),
            SerializedOperation::Insert(text) => Operation::Insert(text.into()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializedSelection {
    ranges: Vec<SerializedRange>,
    primary_index: usize,
}

impl From<&Selection> for SerializedSelection {
    fn from(selection: &Selection) -> Self {
        Self {
            ranges: selection
                .ranges()
                .iter()
                .map(|range| SerializedRange {
                    anchor: range.anchor,
                    head: range.head,
                    old_visual_position: range.old_visual_position,
                })
                .collect(),
            primary_index: selection.primary_index(),
        }
    }
}

impl SerializedSelection {
    fn into_selection(self) -> Result<Selection, InvalidHistory> {
        // `Selection::new` panics on an empty range list and debug-asserts the
        // primary index, so a corrupted file has to be rejected before it.
        if self.ranges.is_empty() {
            return Err(InvalidHistory::new("selection has no ranges"));
        }
        if self.primary_index >= self.ranges.len() {
            return Err(InvalidHistory::new("selection primary index is out of bounds"));
        }

        let ranges: SmallVec<[Range; 1]> = self
            .ranges
            .into_iter()
            .map(|range| Range {
                anchor: range.anchor,
                head: range.head,
                old_visual_position: range.old_visual_position,
            })
            .collect();

        Ok(Selection::new(ranges, self.primary_index))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializedRange {
    anchor: usize,
    head: usize,
    old_visual_position: Option<(u32, u32)>,
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p helix-core --lib transaction::persist`
Expected: PASS, 5 tests.

- [ ] **Step 6: Commit**

```bash
git add helix-core/src/transaction.rs helix-core/src/transaction/persist.rs helix-core/src/history.rs helix-core/src/history/persist.rs
git commit -m "feat(core): serialize transactions for persistent undo"
```

---

### Task 2: History serialization in helix-core

**Files:**
- Modify: `helix-core/src/history/persist.rs` (created in Task 1, currently holds only `InvalidHistory`)
- Modify: `helix-core/src/history.rs` (extend the `pub use` added in Task 1)
- Test: `helix-core/src/history/persist.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `crate::transaction::persist::SerializedTransaction` (Task 1), with `From<&Transaction>` and `into_transaction() -> Result<Transaction, InvalidHistory>`.
- Produces:
  - `pub struct SerializedHistory` with a `pub current: usize` field, `Serialize` and `Deserialize`.
  - `impl History { pub fn to_serialized(&self, current: usize) -> SerializedHistory }`
  - `impl History { pub fn from_serialized(serialized: SerializedHistory) -> Result<History, InvalidHistory> }`
  - Both re-exported from `helix_core::history`.

- [ ] **Step 1: Write the failing test**

Append to `helix-core/src/history/persist.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{history::State, Rope, Selection, Transaction};

    /// Applies a change, committing it to the history first so that the
    /// inversion is recorded against the pre-change document.
    fn commit(history: &mut History, state: &mut State, from: usize, to: usize, text: &str) {
        let transaction = Transaction::change(&state.doc, [(from, to, Some(text.into()))].into_iter())
            .with_selection(Selection::point(from));
        history.commit_revision(&transaction, state);
        transaction.apply(&mut state.doc);
    }

    /// "hello\n" with " world" appended, undone, then "!" appended instead:
    /// revision 2 is a second child of the root, so the history branches.
    fn branching_history() -> (History, State) {
        let mut state = State {
            doc: Rope::from("hello\n"),
            selection: Selection::point(0),
        };
        let mut history = History::default();

        commit(&mut history, &mut state, 5, 5, " world");
        let undo = history.undo().unwrap().clone();
        undo.apply(&mut state.doc);
        commit(&mut history, &mut state, 5, 5, "!");

        (history, state)
    }

    fn roundtrip(history: &History, current: usize) -> History {
        let json = serde_json::to_string(&history.to_serialized(current)).unwrap();
        History::from_serialized(serde_json::from_str(&json).unwrap()).unwrap()
    }

    #[test]
    fn roundtrips_a_branching_history() {
        let (history, _state) = branching_history();
        let mut restored = roundtrip(&history, 2);
        assert_eq!(restored.current_revision(), 2);

        let mut doc = Rope::from("hello!\n");
        let undo = restored.undo().unwrap().clone();
        undo.apply(&mut doc);
        assert_eq!(doc, Rope::from("hello\n"));

        // Redo has to follow the *latest* branch, not the abandoned " world" one.
        let redo = restored.redo().unwrap().clone();
        redo.apply(&mut doc);
        assert_eq!(doc, Rope::from("hello!\n"));
    }

    #[test]
    fn restores_the_revision_that_was_on_disk_not_the_latest_one() {
        let (history, _state) = branching_history();
        // A save that started at revision 1 while the buffer moved on to 2.
        let restored = roundtrip(&history, 1);
        assert_eq!(restored.current_revision(), 1);
    }

    #[test]
    fn rejects_an_empty_history() {
        let mut serialized = branching_history().0.to_serialized(0);
        serialized.revisions.clear();
        assert!(History::from_serialized(serialized).is_err());
    }

    #[test]
    fn rejects_a_current_revision_out_of_bounds() {
        let (history, _state) = branching_history();
        let mut serialized = history.to_serialized(2);
        serialized.current = 99;
        assert!(History::from_serialized(serialized).is_err());
    }

    #[test]
    fn rejects_a_parent_pointing_forward() {
        // `lowest_common_ancestor` walks parents until they meet and relies on
        // them strictly decreasing; a forward pointer makes it loop forever.
        let (history, _state) = branching_history();
        let mut serialized = history.to_serialized(2);
        serialized.revisions[1].parent = 2;
        assert!(History::from_serialized(serialized).is_err());
    }

    #[test]
    fn rejects_a_last_child_out_of_bounds() {
        let (history, _state) = branching_history();
        let mut serialized = history.to_serialized(2);
        serialized.revisions[0].last_child = Some(99);
        assert!(History::from_serialized(serialized).is_err());
    }

    #[test]
    fn restored_timestamps_precede_later_commits() {
        // Restored revisions must land in the past. If they landed in the
        // future, every new commit would sort before them and `:earlier` and
        // `:later` would walk the history in the wrong order.
        let (history, mut state) = branching_history();
        let mut restored = roundtrip(&history, 2);
        let restored_latest = restored.revisions.last().unwrap().timestamp;

        commit(&mut restored, &mut state, 0, 0, "x");
        assert!(restored.revisions.last().unwrap().timestamp >= restored_latest);
    }

    #[test]
    fn restored_timestamps_keep_their_order() {
        let (history, _state) = branching_history();
        let restored = roundtrip(&history, 2);
        for pair in restored.revisions.windows(2) {
            assert!(pair[0].timestamp <= pair[1].timestamp);
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p helix-core --lib history::persist`
Expected: FAIL — `no function or associated item named to_serialized found for struct History`.

- [ ] **Step 3: Write the implementation**

In `helix-core/src/history/persist.rs`, insert above the `InvalidHistory` definition:

```rust
use std::num::NonZeroUsize;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::{History, Revision};
use crate::transaction::persist::SerializedTransaction;

/// The plain-data form of a [`History`], ready to be serialized.
///
/// `current` is the revision matching the document contents the history was
/// stored alongside, which is not necessarily the revision the buffer was on
/// when the history was written: saving is asynchronous and the buffer may have
/// moved on in the meantime.
#[derive(Debug, Serialize, Deserialize)]
pub struct SerializedHistory {
    pub current: usize,
    revisions: Vec<SerializedRevision>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializedRevision {
    parent: usize,
    last_child: Option<usize>,
    transaction: SerializedTransaction,
    inversion: SerializedTransaction,
    timestamp_unix_ms: u64,
}

impl History {
    /// Converts the history into plain data, pointing `current` at the revision
    /// whose contents reached the disk.
    pub fn to_serialized(&self, current: usize) -> SerializedHistory {
        // Revisions are timed with `Instant`, which has no absolute reference
        // point, so they are converted to wall clock time against a single
        // reference pair taken here.
        let reference_instant = Instant::now();
        let reference_system = SystemTime::now();

        debug_assert!(current < self.revisions.len());
        let current = current.min(self.revisions.len() - 1);

        SerializedHistory {
            current,
            revisions: self
                .revisions
                .iter()
                .map(|revision| SerializedRevision {
                    parent: revision.parent,
                    last_child: revision.last_child.map(NonZeroUsize::get),
                    transaction: SerializedTransaction::from(&revision.transaction),
                    inversion: SerializedTransaction::from(&revision.inversion),
                    timestamp_unix_ms: unix_millis(
                        reference_system,
                        reference_instant,
                        revision.timestamp,
                    ),
                })
                .collect(),
        }
    }

    /// Rebuilds a history from plain data, rejecting anything that would make
    /// the revision graph unsound.
    pub fn from_serialized(serialized: SerializedHistory) -> Result<Self, InvalidHistory> {
        if serialized.revisions.is_empty() {
            return Err(InvalidHistory::new("history has no revisions"));
        }
        if serialized.current >= serialized.revisions.len() {
            return Err(InvalidHistory::new("current revision is out of bounds"));
        }

        let len = serialized.revisions.len();
        let timestamps = restore_timestamps(&serialized.revisions);
        let mut revisions = Vec::with_capacity(len);

        for (index, (revision, timestamp)) in serialized
            .revisions
            .into_iter()
            .zip(timestamps)
            .enumerate()
        {
            // The root is its own parent; everything else must point strictly
            // backwards, or `lowest_common_ancestor` never terminates.
            let parent_is_sound = if index == 0 {
                revision.parent == 0
            } else {
                revision.parent < index
            };
            if !parent_is_sound {
                return Err(InvalidHistory::new("revision parent is out of bounds"));
            }
            if revision.last_child.is_some_and(|child| child >= len) {
                return Err(InvalidHistory::new("revision last child is out of bounds"));
            }

            revisions.push(Revision {
                parent: revision.parent,
                last_child: revision.last_child.and_then(NonZeroUsize::new),
                transaction: revision.transaction.into_transaction()?,
                inversion: revision.inversion.into_transaction()?,
                timestamp,
            });
        }

        // A struct literal, so that a field added to `History` upstream is a
        // compile error here.
        Ok(Self {
            revisions,
            current: serialized.current,
        })
    }
}

/// Converts a monotonic `Instant` into wall clock milliseconds, using a
/// reference pair sampled at the same moment.
fn unix_millis(
    reference_system: SystemTime,
    reference_instant: Instant,
    timestamp: Instant,
) -> u64 {
    let age = reference_instant.saturating_duration_since(timestamp);
    reference_system
        .checked_sub(age)
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since_epoch| {
            since_epoch.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

/// Maps stored wall clock timestamps back onto the monotonic clock.
///
/// On Linux and macOS `Instant` counts from boot, so `Instant::now()` minus a
/// week is not representable on a machine that has been up for two days — the
/// common case for a history that survived a reboot. The largest representable
/// offset is probed once and everything older collapses onto it, which keeps
/// revisions ordered at the cost of precision beyond the current uptime.
fn restore_timestamps(revisions: &[SerializedRevision]) -> Vec<Instant> {
    const PROBES: [Duration; 6] = [
        Duration::from_secs(30 * 24 * 60 * 60),
        Duration::from_secs(7 * 24 * 60 * 60),
        Duration::from_secs(24 * 60 * 60),
        Duration::from_secs(60 * 60),
        Duration::from_secs(60),
        Duration::ZERO,
    ];

    let now_instant = Instant::now();
    let now_system = SystemTime::now();
    let earliest = PROBES
        .iter()
        .find_map(|probe| now_instant.checked_sub(*probe))
        .unwrap_or(now_instant);

    revisions
        .iter()
        .map(|revision| {
            let timestamp = UNIX_EPOCH
                .checked_add(Duration::from_millis(revision.timestamp_unix_ms))
                .unwrap_or(now_system);
            let age = now_system
                .duration_since(timestamp)
                .unwrap_or(Duration::ZERO);
            now_instant
                .checked_sub(age)
                .filter(|instant| *instant >= earliest)
                .unwrap_or(earliest)
        })
        .collect()
}
```

Extend the re-export in `helix-core/src/history.rs` to:

```rust
pub use persist::{InvalidHistory, SerializedHistory};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p helix-core --lib history::persist`
Expected: PASS, 8 tests.

- [ ] **Step 5: Run the whole helix-core suite**

Run: `cargo test -p helix-core`
Expected: PASS — nothing in helix-core changed behaviourally, so any failure here means a mistake in the two `mod` lines.

- [ ] **Step 6: Commit**

```bash
git add helix-core/src/history.rs helix-core/src/history/persist.rs
git commit -m "feat(core): serialize undo history to plain data"
```

---

### Task 3: Undo file storage in helix-view

**Files:**
- Create: `helix-view/src/persistent_undo.rs`
- Modify: `helix-view/Cargo.toml` (add `sha2 = "0.11"` next to the other direct dependencies)
- Modify: `helix-view/src/lib.rs` (add `pub mod persistent_undo;` to the module list)
- Test: `helix-view/src/persistent_undo.rs` (inline `#[cfg(test)] mod tests`)

This task builds and tests the storage layer on its own. No hooks are registered and no `Document` is touched yet; that is Task 4.

**Interfaces:**
- Consumes: `helix_core::history::{History, InvalidHistory, SerializedHistory}` (Tasks 1–2).
- Produces:
  - `pub struct PersistentUndoConfig { pub enable: bool, pub dir: Option<PathBuf> }`, `Default` = `{ enable: false, dir: None }`.
  - `fn read(config: &PersistentUndoConfig, path: &Path, text: &Rope) -> Option<History>`
  - `fn write(config: &PersistentUndoConfig, path: &Path, text: &Rope, history: SerializedHistory)`
  - Both private to the module; Task 4 calls them from the hooks in the same file.

- [ ] **Step 1: Write the failing test**

Create `helix-view/src/persistent_undo.rs` containing only this test module:

```rust
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

        write(&config, path, &Rope::from("hello world\n"), history().to_serialized(1));

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

        write(&config, path, &Rope::from("hello world\n"), history().to_serialized(1));

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

        write(&config, path, &Rope::from("hello world\n"), history().to_serialized(1));

        // Undo files hold text deleted from the document, including text the
        // document itself no longer contains.
        let mode = fs::metadata(undo_file(&config, path))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Add `sha2 = "0.11"` to `helix-view/Cargo.toml` and `pub mod persistent_undo;` to `helix-view/src/lib.rs` first, otherwise the module is not compiled at all.

Run: `cargo test -p helix-view --lib persistent_undo`
Expected: FAIL — `cannot find function write in this scope`.

- [ ] **Step 3: Write the implementation**

Prepend to `helix-view/src/persistent_undo.rs`, above the test module:

```rust
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p helix-view --lib persistent_undo`
Expected: PASS, 9 tests on unix, 8 elsewhere.

- [ ] **Step 5: Commit**

```bash
git add helix-view/Cargo.toml helix-view/src/lib.rs helix-view/src/persistent_undo.rs Cargo.lock
git commit -m "feat(view): store undo history in the data directory"
```

---

### Task 4: Wire the feature into the editor

**Files:**
- Modify: `helix-view/src/events.rs` (append `DocumentDidSave` to the `events!` block)
- Modify: `helix-view/src/editor.rs` (config field, `Default` entry, dispatch in `flush_writes`)
- Modify: `helix-view/src/handlers.rs` (call `register_hooks`)
- Modify: `helix-view/src/persistent_undo.rs` (add the hooks)
- Modify: `helix-term/src/events.rs` (register the event)
- Modify: `helix-term/src/application.rs` (dispatch in `handle_document_write`)
- Modify: `helix-term/tests/integration.rs` (declare the test module)
- Test: `helix-term/tests/test/persistent_undo.rs`

**Interfaces:**
- Consumes: `read`, `write`, `PersistentUndoConfig` (Task 3); `History::to_serialized` (Task 2).
- Produces:
  - `helix_view::events::DocumentDidSave<'a> { editor: &'a mut Editor, doc: DocumentId, revision: usize, text: &'a Rope }`
  - `helix_view::persistent_undo::register_hooks()`
  - `helix_view::editor::Config::persistent_undo: PersistentUndoConfig`

- [ ] **Step 1: Write the failing integration test**

Create `helix-term/tests/test/persistent_undo.rs`:

```rust
use std::fs;
use std::path::Path;

use helix_view::{doc, persistent_undo::PersistentUndoConfig};

use super::*;

/// A test config with persistent undo enabled, pointed at a temporary
/// directory so the tests never touch the real data directory.
fn persistent_undo_config(undo_dir: &Path) -> helix_term::config::Config {
    let mut config = helpers::test_config();
    config.editor.persistent_undo = PersistentUndoConfig {
        enable: true,
        dir: Some(undo_dir.to_path_buf()),
    };
    config
}

#[tokio::test(flavor = "multi_thread")]
async fn test_undo_history_survives_a_restart() -> anyhow::Result<()> {
    let undo_dir = tempfile::tempdir()?;
    let file = helpers::temp_file_with_contents("hello\n")?;

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(&mut app, Some("A world<esc>:w<ret>"), None, false).await?;

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(
        &mut app,
        Some("u"),
        Some(&|app| {
            let doc = doc!(app.editor);
            assert_eq!(doc.text().to_string(), "hello\n");
        }),
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_undo_history_is_persisted_when_exiting() -> anyhow::Result<()> {
    // `:x` drains the save queue through `Editor::flush_writes`, which never
    // reaches `Application::handle_document_write`.
    let undo_dir = tempfile::tempdir()?;
    let file = helpers::temp_file_with_contents("hello\n")?;

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(&mut app, Some("A world<esc>:x<ret>"), None, true).await?;

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(
        &mut app,
        Some("u"),
        Some(&|app| {
            let doc = doc!(app.editor);
            assert_eq!(doc.text().to_string(), "hello\n");
        }),
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_editing_continues_into_restored_history() -> anyhow::Result<()> {
    let undo_dir = tempfile::tempdir()?;
    let file = helpers::temp_file_with_contents("hello\n")?;

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(&mut app, Some("A world<esc>:w<ret>"), None, false).await?;

    // A new edit, then two undos: the first walks back over this session's
    // edit, the second has to cross into the restored history.
    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(
        &mut app,
        Some("A!<esc>uu"),
        Some(&|app| {
            let doc = doc!(app.editor);
            assert_eq!(doc.text().to_string(), "hello\n");
        }),
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_undo_history_is_discarded_when_the_file_changed() -> anyhow::Result<()> {
    let undo_dir = tempfile::tempdir()?;
    let file = helpers::temp_file_with_contents("hello\n")?;

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(&mut app, Some("A world<esc>:w<ret>"), None, false).await?;

    fs::write(file.path(), "something else\n")?;

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(
        &mut app,
        Some("u"),
        Some(&|app| {
            let doc = doc!(app.editor);
            assert_eq!(doc.text().to_string(), "something else\n");
        }),
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_undo_history_follows_a_save_as() -> anyhow::Result<()> {
    let undo_dir = tempfile::tempdir()?;
    let file = helpers::temp_file_with_contents("hello\n")?;
    let other = tempfile::NamedTempFile::new()?;

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(file.path(), None)
        .build()?;
    let write_as = format!("A world<esc>:w {}<ret>", other.path().to_string_lossy());
    test_key_sequence(&mut app, Some(&write_as), None, false).await?;

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(other.path(), None)
        .build()?;
    test_key_sequence(
        &mut app,
        Some("u"),
        Some(&|app| {
            let doc = doc!(app.editor);
            assert_eq!(doc.text().to_string(), "hello\n");
        }),
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_saving_succeeds_when_the_undo_directory_is_unusable() -> anyhow::Result<()> {
    // A plain file where the undo directory should be. Persisting the history
    // fails; saving the document must not.
    let undo_dir = tempfile::tempdir()?;
    let blocked = undo_dir.path().join("undo");
    fs::write(&blocked, "not a directory")?;

    let file = helpers::temp_file_with_contents("hello\n")?;
    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(&blocked))
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(&mut app, Some("A world<esc>:w<ret>"), None, false).await?;

    assert_eq!(fs::read_to_string(file.path())?, "hello world\n");

    Ok(())
}
```

Add to `helix-term/tests/integration.rs`, in the `mod test { ... }` block alongside the other module declarations:

```rust
    mod persistent_undo;
```

`AppBuilder::with_config` in `helix-term/tests/test/helpers.rs` carries an
`#[allow(dead_code)]` with a comment asking for it to be removed once a test
uses it. These tests use it, so delete both the attribute and the comment.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p helix-term --test integration persistent_undo`
Expected: FAIL — `no field persistent_undo on type helix_view::editor::Config`.

- [ ] **Step 3: Add the event**

Append inside the `events!` block in `helix-view/src/events.rs`, after `DocumentFocusLost`:

```rust
    // called **after** a document's contents have reached the disk
    DocumentDidSave<'a> {
        editor: &'a mut Editor,
        doc: DocumentId,
        /// The revision whose contents were written.
        revision: usize,
        /// The text that was written, which is not necessarily the document's
        /// current text: saving is asynchronous.
        text: &'a Rope
    }
```

In `helix-term/src/events.rs`, add `DocumentDidSave` to the existing `use helix_view::events::{...}` list and add to `register()`:

```rust
    register_event::<DocumentDidSave>();
```

- [ ] **Step 4: Dispatch from both save paths**

In `helix-term/src/application.rs`, at the very end of `handle_document_write`, after the `set_status` call:

```rust
        helix_event::dispatch(helix_view::events::DocumentDidSave {
            editor: &mut self.editor,
            doc: doc_save_event.doc_id,
            revision: doc_save_event.revision,
            text: &doc_save_event.text,
        });
```

In `helix-view/src/editor.rs`, in `flush_writes`, directly after the existing `doc.set_last_saved_revision(...)` call:

```rust
                // `:x`, `:wq` and `:wqa` drain the queue here instead of going
                // through `Application::handle_document_write`, so the event has
                // to be dispatched from both places.
                helix_event::dispatch(DocumentDidSave {
                    editor: self,
                    doc: save_event.doc_id,
                    revision: save_event.revision,
                    text: &save_event.text,
                });
```

Extend the existing events import at the top of `helix-view/src/editor.rs` to include `DocumentDidSave`:

```rust
    events::{DocumentDidClose, DocumentDidOpen, DocumentDidSave, DocumentFocusLost},
```

- [ ] **Step 5: Add the config field**

In `helix-view/src/editor.rs`, add to the `Config` struct after `workspace_trust`:

```rust
    /// Persistent undo history configuration.
    pub persistent_undo: PersistentUndoConfig,
```

Add to `Config::default()` after `workspace_trust: WorkspaceTrustConfig::default(),`:

```rust
            persistent_undo: PersistentUndoConfig::default(),
```

Add to the imports at the top of the file:

```rust
use crate::persistent_undo::PersistentUndoConfig;
```

- [ ] **Step 6: Add the hooks**

Append to `helix-view/src/persistent_undo.rs`, above the test module:

```rust
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

    // Without this the document would look modified the moment it was opened,
    // since its current revision no longer matches the root. The file's mtime
    // rather than the current time, so that the external-modification guard in
    // `Document::save_impl` keeps working.
    let save_time = path
        .metadata()
        .and_then(|metadata| metadata.modified())
        .unwrap_or_else(|_| SystemTime::now());
    doc.set_last_saved_revision(current, save_time);
}

/// Stores a document's history after it has been written to disk.
fn persist(editor: &mut Editor, doc_id: DocumentId, revision: usize, text: &Rope) {
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

    // The history lives in a `Cell` because parts of it are handed out by
    // reference elsewhere; take it out and put it straight back.
    let history = doc.history.take();
    let serialized = history.to_serialized(revision);
    doc.history.set(history);

    write(&config, &path, text, serialized);
}

pub fn register_hooks() {
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        restore(event.editor, event.doc);
        Ok(())
    });

    register_hook!(move |event: &mut DocumentDidSave<'_>| {
        persist(event.editor, event.doc, event.revision, event.text);
        Ok(())
    });
}
```

Extend the module's imports with:

```rust
use std::time::SystemTime;

use helix_event::register_hook;

use crate::events::{DocumentDidOpen, DocumentDidSave};
use crate::{DocumentId, Editor};
```

Add to `register_hooks` in `helix-view/src/handlers.rs`:

```rust
    crate::persistent_undo::register_hooks();
```

- [ ] **Step 7: Run the integration tests to verify they pass**

Run: `cargo test -p helix-term --test integration persistent_undo`
Expected: PASS, 6 tests.

- [ ] **Step 8: Run the full suite**

Run: `cargo test --workspace`
Expected: PASS. The feature is off by default, so no existing test should change behaviour.

- [ ] **Step 9: Check formatting and lints**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: no output from fmt, no warnings from clippy.

- [ ] **Step 10: Commit**

```bash
git add helix-view/src/events.rs helix-view/src/editor.rs helix-view/src/handlers.rs helix-view/src/persistent_undo.rs helix-term/src/events.rs helix-term/src/application.rs helix-term/tests/integration.rs helix-term/tests/test/persistent_undo.rs
git commit -m "feat: restore and persist undo history across sessions"
```

---

### Task 5: Document the option

**Files:**
- Modify: `book/src/editor.md`

- [ ] **Step 1: Add the section**

Append a section to `book/src/editor.md`, following the formatting of the neighbouring nested-section entries (a `### [editor.<name>] Section` heading, a sentence of prose, a key table, and an example):

````markdown
### `[editor.persistent-undo]` Section

Keeps a document's undo history across editing sessions. With this enabled,
reopening a file and pressing `u` walks back through edits made before the
editor was last closed.

History is written when a document is saved and is bound to the contents that
were written. If the file changes outside the editor — a `git checkout`, another
editor, a formatter — the stored history no longer describes it and is discarded
the next time the file is opened.

| Key | Description | Default |
| --- | ----------- | ------- |
| `enable` | Whether to keep undo history across sessions | `false` |
| `dir` | Where to keep undo files | the `undo` directory inside Helix's [data directory](#data-directory) |

Undo files record text that was deleted from the document, so they hold content
the document itself no longer contains. On unix they are created readable only
by their owner; on a shared machine, consider whether the directory they live in
deserves the same treatment.

Example:

```toml
[editor.persistent-undo]
enable = true
```
````

- [ ] **Step 2: Verify the book builds**

Run: `mdbook build book`
Expected: builds without errors. If `mdbook` is not installed, skip this step and instead confirm that the fenced code blocks and the table render correctly by reading the file.

- [ ] **Step 3: Commit**

```bash
git add book/src/editor.md
git commit -m "docs: document the persistent-undo option"
```

---

## Amendments made during execution

The code blocks above are the plan as written before implementation. Five
defects in them were found by task reviews and corrected in the shipped code.
They are recorded here rather than edited into the blocks above, so that the
plan stays a faithful record of what was planned and what changed.

1. **Task 1 — `into_change_set` used `+=` to recompute `len`/`len_after`.** Those
   operands come straight out of a file. Under the workspace's default test
   profile the addition panics on overflow; in release it wraps silently, which
   can let a corrupt change set pass the length check and panic later inside
   `ChangeSet::apply`. Shipped with `checked_add` and an overflow rejection.

2. **Task 2 — `from_serialized` did not validate every invariant the rest of
   `history.rs` assumes.** `last_edit_pos` unconditionally expects a non-root
   revision's inversion to carry a selection and its transaction to yield at
   least one change, and panics on `g;` otherwise. `last_child` was bounds-checked
   but not required to point forward or to agree with its target's parent.
   All four checks shipped; each is guaranteed by `commit_revision_at_timestamp`,
   so none can reject a legitimate history.

3. **Task 2 — `restore_timestamps` did not guarantee ordering.** `jump_instant`
   binary-searches revisions by timestamp. Shipped with a running maximum that
   clamps the sequence non-decreasing, rather than rejecting the file: timestamps
   are the least valuable part of a history to discard it over.

4. **Task 3 — the version-mismatch discard logged at `debug`.** The design doc
   specifies `warn`, and a format bump discards every user's history at once.
   Shipped at `warn`. The content-hash mismatch stays at `debug`, since a file
   changing on disk is an ordinary event.

5. **Task 4 — `persist` read `doc.path()`.** `:wq other.txt` and `:x other.txt`
   route only through `Editor::flush_writes`, which never calls `set_doc_path`,
   so the history was keyed to the old path: the new file got none, and the old
   file's undo file was overwritten with a mismatched hash. Shipped with the
   written path threaded through `DocumentDidSave` and used directly.

The plan's own documentation task also linked to a `#data-directory` anchor that
does not exist anywhere in the book. The shipped documentation states the default
in prose without a link, matching `book/src/workspace-trust.md`.
