//! Persistence of undo history. See `docs/superpowers/specs/2026-09-26-persistent-undo-design.md`.

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

        // Validate the graph's shape against the raw indices before anything is
        // consumed or converted. `last_child` is checked here, rather than
        // alongside `parent` below, because confirming it agrees with the
        // target's `parent` needs random access into revisions the forward
        // build loop below has not reached (and, for a child index, has not
        // yet built) when it is looking at an earlier one.
        for (index, revision) in serialized.revisions.iter().enumerate() {
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

            if let Some(child) = revision.last_child {
                // A last child must be a later revision. One pointing at itself,
                // an ancestor, or an unrelated earlier revision would silently
                // corrupt `redo`, which applies `revisions[last_child].transaction`
                // unconditionally.
                if child <= index || child >= len {
                    return Err(InvalidHistory::new("revision last child is out of bounds"));
                }
                // The pointer must be reciprocated. Without this, `last_child`
                // and `parent` can disagree about the shape of the tree even
                // though both are individually in bounds.
                if serialized.revisions[child].parent != index {
                    return Err(InvalidHistory::new(
                        "revision last child does not agree with its parent",
                    ));
                }
            }
        }

        let timestamps = restore_timestamps(&serialized.revisions);
        let mut revisions = Vec::with_capacity(len);

        for (index, (revision, timestamp)) in
            serialized.revisions.into_iter().zip(timestamps).enumerate()
        {
            let transaction = revision.transaction.into_transaction()?;
            let inversion = revision.inversion.into_transaction()?;

            // Every revision but the root was committed through
            // `commit_revision_at_timestamp` (history.rs), which always gives the
            // inversion a selection and always records at least one change; the
            // root's transaction and inversion are legitimately empty and
            // selection-less. `last_edit_pos` (history.rs) assumes both
            // unconditionally for any non-root current revision and panics
            // otherwise, reachable through the ordinary `goto_last_modification`
            // command.
            if index != 0 {
                if inversion.selection().is_none() {
                    return Err(InvalidHistory::new("revision inversion has no selection"));
                }
                if transaction.changes_iter().next().is_none() {
                    return Err(InvalidHistory::new("revision transaction has no changes"));
                }
            }

            revisions.push(Revision {
                parent: revision.parent,
                last_child: revision.last_child.and_then(NonZeroUsize::new),
                transaction,
                inversion,
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

    // `jump_instant`'s binary search (history.rs) assumes revisions are sorted
    // by timestamp, but a corrupted file's `timestamp_unix_ms` values need not
    // be in order. Rather than rejecting an otherwise-valid history over its
    // least load-bearing field, ordering is enforced by construction: each
    // restored instant is clamped to be at least the previous one.
    let mut running_max = earliest;
    revisions
        .iter()
        .map(|revision| {
            let timestamp = UNIX_EPOCH
                .checked_add(Duration::from_millis(revision.timestamp_unix_ms))
                .unwrap_or(now_system);
            let age = now_system
                .duration_since(timestamp)
                .unwrap_or(Duration::ZERO);
            let candidate = now_instant
                .checked_sub(age)
                .filter(|instant| *instant >= earliest)
                .unwrap_or(earliest);
            running_max = running_max.max(candidate);
            running_max
        })
        .collect()
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{history::State, Rope, Selection, Transaction};

    /// Applies a change, committing it to the history first so that the
    /// inversion is recorded against the pre-change document.
    fn commit(history: &mut History, state: &mut State, from: usize, to: usize, text: &str) {
        let transaction =
            Transaction::change(&state.doc, [(from, to, Some(text.into()))].into_iter())
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
    fn rejects_a_last_child_pointing_backward() {
        // A last child must be a later revision; one pointing at an ancestor
        // (or itself) would silently corrupt `redo`, which applies
        // `revisions[last_child].transaction` unconditionally.
        let (history, _state) = branching_history();
        let mut serialized = history.to_serialized(2);
        serialized.revisions[2].last_child = Some(0);
        assert!(History::from_serialized(serialized).is_err());
    }

    #[test]
    fn rejects_a_last_child_whose_target_disagrees_about_its_parent() {
        // Revision 1 claims revision 2 as its child, but revision 2's own
        // `parent` still points at the root: `last_child` and `parent` disagree
        // about the shape of the tree even though both are individually in
        // bounds and correctly ordered.
        let (history, _state) = branching_history();
        let mut serialized = history.to_serialized(2);
        serialized.revisions[1].last_child = Some(2);
        assert!(History::from_serialized(serialized).is_err());
    }

    #[test]
    fn rejects_a_non_root_inversion_without_a_selection() {
        // `last_edit_pos` (history.rs) unconditionally unwraps the current
        // revision's inversion selection for any non-root current revision;
        // every legitimately committed non-root revision has one, but nothing
        // stops a corrupted file from setting it to `null`.
        let (history, _state) = branching_history();
        let mut json = serde_json::to_value(history.to_serialized(2)).unwrap();
        json["revisions"][1]["inversion"]["selection"] = serde_json::Value::Null;
        let serialized: SerializedHistory = serde_json::from_value(json).unwrap();
        let error = History::from_serialized(serialized).unwrap_err();
        assert!(error.to_string().contains("selection"));
    }

    #[test]
    fn rejects_a_non_root_transaction_with_no_changes() {
        // `last_edit_pos` also unwraps the first item of the current revision's
        // `changes_iter()`, which yields nothing for a change set made only of
        // `Retain` operations.
        let (history, _state) = branching_history();
        let mut json = serde_json::to_value(history.to_serialized(2)).unwrap();
        let len = json["revisions"][1]["transaction"]["changes"]["len"]
            .as_u64()
            .unwrap();
        json["revisions"][1]["transaction"]["changes"]["changes"] =
            serde_json::json!([{ "Retain": len }]);
        json["revisions"][1]["transaction"]["changes"]["len_after"] = serde_json::json!(len);
        let serialized: SerializedHistory = serde_json::from_value(json).unwrap();
        let error = History::from_serialized(serialized).unwrap_err();
        assert!(error.to_string().contains("no changes"));
    }

    #[test]
    fn restore_timestamps_produces_a_non_decreasing_sequence_even_when_scrambled() {
        // `jump_instant`'s binary search assumes revisions are sorted by
        // timestamp. A corrupted file need not have ordered `timestamp_unix_ms`
        // values, so the restored sequence must be non-decreasing regardless.
        let (history, _state) = branching_history();
        let mut serialized = history.to_serialized(2);
        let len = serialized.revisions.len();
        for (index, revision) in serialized.revisions.iter_mut().enumerate() {
            // Deliberately scrambled: later revisions claim earlier timestamps.
            revision.timestamp_unix_ms = (len - index) as u64 * 1000;
        }

        let timestamps = restore_timestamps(&serialized.revisions);
        for pair in timestamps.windows(2) {
            assert!(pair[0] <= pair[1]);
        }
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
