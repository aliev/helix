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
    // `pub(crate)`, not `pub`: helix-view treats `SerializedHistory` opaquely,
    // only ever obtaining one from `to_serialized` and handing it to
    // `from_serialized` or to `serde`, neither of which needs field access.
    pub(crate) current: usize,
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
    ///
    /// Returns `None` if `current` is not a valid revision of this history,
    /// rather than panicking or clamping it to one: a clamped `current` would
    /// silently pair the written text with whatever revision happened to be
    /// last, which is the one wrong answer a caller can least afford here.
    /// Callers such as `persistent_undo::persist` are expected to treat `None`
    /// as "do not persist" and log accordingly.
    pub fn to_serialized(&self, current: usize) -> Option<SerializedHistory> {
        if current >= self.revisions.len() {
            return None;
        }

        // Revisions are timed with `Instant`, which has no absolute reference
        // point, so they are converted to wall clock time against a single
        // reference pair taken here.
        let reference_instant = Instant::now();
        let reference_system = SystemTime::now();

        Some(SerializedHistory {
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
        })
    }

    /// Rebuilds a history from plain data, rejecting anything that would make
    /// the revision graph unsound.
    pub fn from_serialized(
        mut serialized: SerializedHistory,
        max_bytes: usize,
    ) -> Result<Self, InvalidHistory> {
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

            // Length invariants guaranteed by `commit_revision_at_timestamp`
            // (history.rs) for every non-root revision, checked here against
            // the raw serialized lengths, before either change set is turned
            // into a real `Transaction` that `apply` would trust. None of this
            // is caught by `SerializedChangeSet::into_change_set`, which only
            // checks a change set against its own operation list.
            if index != 0 {
                let (transaction_len, transaction_len_after) =
                    revision.transaction.change_set_lengths();
                let (inversion_len, inversion_len_after) = revision.inversion.change_set_lengths();

                // `Transaction::invert` builds the inversion by walking the
                // transaction's own operations, which makes its length the
                // transaction's length-after and its length-after the
                // transaction's length: it is the same document transition,
                // run backwards. A mismatch here could not have come from
                // `commit_revision_at_timestamp`, only from a hand-edited or
                // corrupted file.
                if inversion_len != transaction_len_after || inversion_len_after != transaction_len
                {
                    return Err(InvalidHistory::new(
                        "revision inversion length disagrees with its transaction",
                    ));
                }

                // A revision's transaction is built against the document as it
                // stood at its parent revision, so its required length must
                // match the length the parent's transaction produces. The root
                // is exempt: its transaction is a dummy empty change set (see
                // `History::default`) that does not describe the document's
                // real initial length.
                if revision.parent != 0 {
                    let (_, parent_len_after) = serialized.revisions[revision.parent]
                        .transaction
                        .change_set_lengths();
                    if transaction_len != parent_len_after {
                        return Err(InvalidHistory::new(
                            "revision transaction length disagrees with its parent's",
                        ));
                    }
                }
            }
        }

        // Trim here: after validation, so the walk below can rely on
        // `parent < index`; before the build loop, so revisions that are about
        // to be dropped are never turned into `Transaction`s in the first place.
        //
        // Renumbering is safe at this exact moment and nowhere else. A revision
        // index is dangerous only while something outside `History` is holding
        // one — `Document::last_saved_revision` and `View::doc_revisions` — and
        // at load time neither exists yet: the document was just built, its
        // last-saved revision is about to be set by the restore hook, and no
        // view has synced with it. That is why this feature needs no changes to
        // `History` itself.
        trim_to_budget(&mut serialized, max_bytes);

        let len = serialized.revisions.len();

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

/// Drops revisions until the kept subtree's text fits `max_bytes`.
///
/// The kept set is always a subtree. A revision's `inversion` reconstructs the
/// text of its parent, so keeping a child whose parent was dropped would make
/// undo produce a document state that never existed.
fn trim_to_budget(serialized: &mut SerializedHistory, max_bytes: usize) {
    let len = serialized.revisions.len();

    // Each revision's own payload, then its subtree total. Parents always
    // precede their children, so one backwards pass accumulates every subtree.
    let own: Vec<usize> = serialized
        .revisions
        .iter()
        .map(|revision| revision.transaction.text_bytes() + revision.inversion.text_bytes())
        .collect();
    let mut subtree = own.clone();
    for index in (1..len).rev() {
        subtree[serialized.revisions[index].parent] += subtree[index];
    }

    let mut kept: Vec<bool> = vec![true; len];
    let current = serialized.current;

    // Descendants of `current` are reachable only by redo. When `current`'s own
    // subtree overflows, shed abandoned branches first — the ones not on the
    // redo chain — highest index first (the highest kept index in a subtree is
    // always a leaf, because children have larger indices than their parents).
    // Only once no abandoned descendant remains does shedding cut into the
    // redo chain itself, furthest step first: `last_child` always names a
    // node's most recently created child (`commit_revision_at_timestamp` in
    // `history.rs`), so the chain from `current` always has the *highest*
    // index among `current`'s descendants. Shedding by raw index alone would
    // drop that reachable branch before an older, permanently abandoned one.
    //
    // The chain and its membership mask are built once, up front, rather than
    // on every iteration: `chain.contains` rescanned the whole chain per
    // candidate, which made a long linear redo chain (undo a long way back,
    // then `:w`) cost O(n) candidates times O(n) `contains` per shed revision
    // — O(n^2) for the loop as a whole. The mask turns membership into an O(1)
    // lookup. Only the `or_else` branch below ever sheds a chain member, and
    // it always sheds `chain`'s current last element (the deepest surviving
    // step, per the ordering `redo_chain` documents), so the chain and its
    // mask can be kept in sync with a plain pop instead of a full rebuild.
    let mut current_subtree = subtree[current];
    let mut chain = redo_chain(&serialized.revisions, &kept, current);
    let mut chain_mask = vec![false; len];
    for &index in &chain {
        chain_mask[index] = true;
    }
    while current_subtree > max_bytes {
        let victim = (current + 1..len)
            .rev()
            .find(|&index| {
                kept[index]
                    && is_descendant_of(&serialized.revisions, index, current)
                    && !chain_mask[index]
            })
            .or_else(|| chain.last().copied());
        match victim {
            Some(index) => {
                kept[index] = false;
                current_subtree -= own[index];
                if chain_mask[index] {
                    // The redo chain was cut: `index` was `chain`'s last
                    // (deepest) element, so dropping it is enough to keep both
                    // in sync with what a fresh `redo_chain` call would return.
                    chain.pop();
                    chain_mask[index] = false;
                }
            }
            // Nothing left to shed: `current` alone exceeds the budget and is
            // kept anyway. A single large edit must not leave an empty history.
            None => break,
        }
    }

    // Walk up from `current` while the ancestor's subtree still fits, keeping
    // as much undo depth as the budget allows.
    let mut root = current;
    loop {
        let parent = serialized.revisions[root].parent;
        if parent == root {
            break;
        }
        // The dropped redo branches above are not reflected in `subtree`, so
        // this can only ever choose a smaller root than strictly necessary —
        // never a larger one, which would overshoot the budget.
        if subtree[parent] > max_bytes {
            break;
        }
        root = parent;
    }

    // The common case: a history comfortably under budget sheds nothing above
    // and walks all the way back to the original root. Checked here, before
    // the discard pass below runs at all, rather than after it: `root == 0`
    // once shedding kept every revision means that pass would not change
    // anything, and for a history of real size — tens of thousands of
    // revisions, accumulated over a long session — it is the overwhelmingly
    // common case. Every other pass in this function is already O(n); doing
    // this check first, rather than last, is the difference between an O(n)
    // open and one that quietly costs O(n^2).
    if root == 0 && kept.iter().all(|&keep| keep) {
        return;
    }

    // Everything outside the chosen root's subtree goes, including abandoned
    // branches that are older than it. This walks ascending rather than
    // calling `is_descendant_of` per index: `parent < index` (validated
    // above) means each index's parent has already been visited and its
    // final membership decided, so `kept[index]` can simply be AND-ed with
    // its parent's. A parent shed above never has a surviving kept child —
    // shedding always removes a current leaf first — so folding this into
    // `kept` in place, instead of computing membership into a separate
    // vector first, never mixes up "shed" with "outside the root's subtree".
    for index in 0..len {
        if index < root {
            kept[index] = false;
        } else if index != root {
            kept[index] = kept[index] && kept[serialized.revisions[index].parent];
        }
    }
    kept[root] = true;

    // Renumber. Old indices are ascending, so the surviving order is preserved
    // and parents still precede their children.
    let mut remap = vec![usize::MAX; len];
    let mut next = 0;
    for index in 0..len {
        if kept[index] {
            remap[index] = next;
            next += 1;
        }
    }

    let mut revisions = Vec::with_capacity(next);
    for (index, mut revision) in std::mem::take(&mut serialized.revisions)
        .into_iter()
        .enumerate()
    {
        if !kept[index] {
            continue;
        }

        if index == root {
            // There is nothing above a root to undo into, so it carries no
            // change of its own — matching the root `History::default` builds.
            revision.parent = 0;
            revision.transaction = SerializedTransaction::empty();
            revision.inversion = SerializedTransaction::empty();
        } else {
            revision.parent = remap[revision.parent];
        }

        revision.last_child = revision
            .last_child
            .filter(|&child| kept[child])
            .map(|child| remap[child]);

        revisions.push(revision);
    }

    serialized.current = remap[current];
    serialized.revisions = revisions;
}

/// The indices reachable from `current` by pressing redo repeatedly: its
/// `last_child`, then that revision's `last_child`, and so on, stopping at the
/// first one no longer `kept`. `last_child` always names a node's most
/// recently created child, so each step strictly increases the index —
/// the chain is furthest-first when read from the end.
fn redo_chain(revisions: &[SerializedRevision], kept: &[bool], current: usize) -> Vec<usize> {
    let mut chain = Vec::new();
    let mut node = current;
    while let Some(child) = revisions[node].last_child {
        if !kept[child] {
            break;
        }
        chain.push(child);
        node = child;
    }
    chain
}

/// Whether `index` is `ancestor` or sits below it in the tree.
fn is_descendant_of(revisions: &[SerializedRevision], index: usize, ancestor: usize) -> bool {
    if index < ancestor {
        return false;
    }
    let mut walk = index;
    // Parents strictly decrease except at the root, which is its own parent, so
    // this terminates for any graph that passed validation.
    loop {
        if walk == ancestor {
            return true;
        }
        let parent = revisions[walk].parent;
        if parent == walk {
            return false;
        }
        walk = parent;
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

    /// "hello\n" with " world" appended, then "!" appended after that with no
    /// undo in between: revision 2's parent is revision 1, not the root,
    /// which `branching_history` above never exercises (both of its non-root
    /// revisions are children of the root).
    fn sequential_history() -> (History, State) {
        let mut state = State {
            doc: Rope::from("hello\n"),
            selection: Selection::point(0),
        };
        let mut history = History::default();

        commit(&mut history, &mut state, 5, 5, " world");
        commit(&mut history, &mut state, 11, 11, "!");

        (history, state)
    }

    /// A budget no test history can reach, for tests about something else.
    const NO_TRIM: usize = usize::MAX;

    fn roundtrip(history: &History, current: usize) -> History {
        let json = serde_json::to_string(&history.to_serialized(current).unwrap()).unwrap();
        History::from_serialized(serde_json::from_str(&json).unwrap(), NO_TRIM).unwrap()
    }

    /// A linear history: "" -> "aaaa" -> "bbbb" -> "cccc", each revision
    /// carrying four bytes of inserted text and four of deleted text in its
    /// inversion.
    fn linear_history() -> (History, State) {
        let mut state = State {
            doc: Rope::from(""),
            selection: Selection::point(0),
        };
        let mut history = History::default();
        for text in ["aaaa", "bbbb", "cccc"] {
            let transaction = Transaction::change(
                &state.doc,
                [(0, state.doc.len_chars(), Some(text.into()))].into_iter(),
            )
            .with_selection(Selection::point(0));
            history.commit_revision(&transaction, &state);
            transaction.apply(&mut state.doc);
        }
        (history, state)
    }

    /// The root with three children, each an 8-byte insertion from the empty
    /// document: revisions 1 and 2 are abandoned (undone past), revision 3 is
    /// `last_child` — the only one reachable by redo. `current` stays at the
    /// root throughout.
    fn root_with_abandoned_and_redo_branches() -> (History, State) {
        let mut state = State {
            doc: Rope::from(""),
            selection: Selection::point(0),
        };
        let mut history = History::default();

        for text in ["aaaaaaaa", "bbbbbbbb", "cccccccc"] {
            commit(&mut history, &mut state, 0, 0, text);
            let undo = history.undo().unwrap().clone();
            undo.apply(&mut state.doc);
        }
        assert_eq!(history.current_revision(), 0);

        (history, state)
    }

    /// `current` (revision 1) with an abandoned 8-byte child (revision 2) and
    /// a two-step redo chain (revisions 3 and 4, 8 bytes each): redo from
    /// `current` reaches 3, then 4.
    fn mid_history_with_abandoned_and_deep_redo_chain() -> (History, State) {
        let mut state = State {
            doc: Rope::from(""),
            selection: Selection::point(0),
        };
        let mut history = History::default();

        // Revision 1: the eventual `current`.
        commit(&mut history, &mut state, 0, 0, "aaaaaaaa");
        // Revision 2: an abandoned attempt from revision 1.
        commit(&mut history, &mut state, 8, 8, "bbbbbbbb");
        let undo = history.undo().unwrap().clone();
        undo.apply(&mut state.doc);
        // Revision 3: the kept branch from revision 1.
        commit(&mut history, &mut state, 8, 8, "cccccccc");
        // Revision 4: one step further down the kept branch.
        commit(&mut history, &mut state, 16, 16, "dddddddd");

        history.undo();
        history.undo();
        assert_eq!(history.current_revision(), 1);

        (history, state)
    }

    #[test]
    fn shedding_drops_abandoned_branches_before_the_redo_chain() {
        // At the root, budget 8 only has room for the root itself plus one
        // 8-byte revision. Both abandoned branches (1 and 2) must go before
        // the redo chain (3) is touched, or reopening the file would silently
        // lose the ability to redo. Against the old highest-raw-index-first
        // ordering, this instead drops revision 3 (the actual redo target,
        // which always has the highest index because `last_child` always
        // names the most recently created child) while keeping an abandoned
        // sibling — exactly the bug in Important 1.
        let (history, _state) = root_with_abandoned_and_redo_branches();
        let mut restored = History::from_serialized(history.to_serialized(0).unwrap(), 8).unwrap();

        assert_eq!(restored.revisions.len(), 2);
        assert_eq!(restored.current_revision(), 0);

        let redo = restored.redo().cloned().expect("redo chain must survive");
        let mut doc = Rope::from("");
        redo.apply(&mut doc);
        assert_eq!(doc, Rope::from("cccccccc"));
    }

    #[test]
    fn trimming_that_reaches_into_the_redo_chain_drops_the_deepest_step_first() {
        // Budget 16 cannot fit the whole 32-byte subtree hanging off
        // `current` (1 revision of 8 bytes for `current` itself, plus 8 for
        // the abandoned branch and 16 for the two-step redo chain). The
        // abandoned branch goes first, then the trim must cut into the redo
        // chain itself, dropping the furthest step (4) before the nearer one
        // (3): redo still works once, but not twice.
        let (history, _state) = mid_history_with_abandoned_and_deep_redo_chain();
        let mut restored = History::from_serialized(history.to_serialized(1).unwrap(), 16).unwrap();

        assert_eq!(restored.revisions.len(), 2);
        assert!(restored.redo().is_some());
        assert!(restored.redo().is_none());
    }

    #[test]
    fn a_history_within_budget_is_untouched() {
        let (history, _state) = linear_history();
        let restored =
            History::from_serialized(history.to_serialized(3).unwrap(), NO_TRIM).unwrap();

        assert_eq!(restored.current_revision(), 3);
        assert_eq!(restored.revisions.len(), 4);
    }

    #[test]
    fn a_history_over_budget_keeps_the_newest_revisions() {
        let (history, _state) = linear_history();
        // Per-revision payload bytes are 0, 4, 8, 8 and subtree totals are 20,
        // 20, 16, 8 (see the worked example): at a 16-byte budget the walk up
        // from `current` stops at revision 2, so exactly revisions 2 and 3
        // survive, renumbered to 0 and 1.
        let mut restored = History::from_serialized(history.to_serialized(3).unwrap(), 16).unwrap();

        assert_eq!(restored.revisions.len(), 2);
        assert_eq!(restored.current_revision(), 1);

        // Undoing once reaches the promoted root; a second undo must refuse.
        assert!(restored.undo().is_some());
        assert_eq!(restored.current_revision(), 0);
        assert!(restored.undo().is_none());
    }

    #[test]
    fn the_promoted_root_carries_no_change() {
        let (history, _state) = linear_history();
        let restored = History::from_serialized(history.to_serialized(3).unwrap(), 16).unwrap();

        let root = &restored.revisions[0];
        assert_eq!(root.parent, 0);
        assert!(root.transaction.changes_iter().next().is_none());
        assert!(root.inversion.changes_iter().next().is_none());
    }

    #[test]
    fn a_trimmed_history_still_undoes_and_stops_at_its_root() {
        let (history, state) = linear_history();
        let mut restored = History::from_serialized(history.to_serialized(3).unwrap(), 16).unwrap();

        let mut doc = state.doc.clone();
        // Undo back to the new root, then confirm it refuses to go further
        // rather than producing text that was never on screen.
        loop {
            let Some(transaction) = restored.undo().cloned() else {
                break;
            };
            assert!(transaction.apply(&mut doc));
        }
        assert_eq!(restored.current_revision(), 0);
        assert!(restored.undo().is_none());
    }

    #[test]
    fn a_zero_budget_keeps_exactly_one_revision() {
        let (history, _state) = linear_history();
        let mut restored = History::from_serialized(history.to_serialized(3).unwrap(), 0).unwrap();

        assert_eq!(restored.revisions.len(), 1);
        assert_eq!(restored.current_revision(), 0);
        assert!(restored.undo().is_none());
    }

    #[test]
    fn trimming_keeps_redo_branches_of_the_current_revision() {
        // Undo to revision 1, so revisions 2 and 3 become a redo branch that
        // hangs off `current`. They must survive, or reopening a file after an
        // undo would silently lose the ability to redo.
        let (mut history, _state) = linear_history();
        history.undo();
        history.undo();
        assert_eq!(history.current_revision(), 1);

        let mut restored =
            History::from_serialized(history.to_serialized(1).unwrap(), NO_TRIM).unwrap();
        assert!(restored.redo().is_some());
    }

    #[test]
    fn a_trimmed_history_round_trips_without_shrinking_again() {
        // The trim's output must satisfy the same validation its input does,
        // and a second pass at the same budget must be a no-op — otherwise
        // every open would erode the history a little further.
        let (history, _state) = linear_history();
        let once = History::from_serialized(history.to_serialized(3).unwrap(), 16).unwrap();
        let once_len = once.revisions.len();
        let once_current = once.current_revision();

        let twice =
            History::from_serialized(once.to_serialized(once_current).unwrap(), 16).unwrap();

        assert_eq!(twice.revisions.len(), once_len);
        assert_eq!(twice.current_revision(), once_current);
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
    fn to_serialized_rejects_a_revision_out_of_range() {
        // The caller (`persistent_undo::persist`) is expected to skip
        // persisting and log a warning on `None` rather than ever panicking or
        // silently pairing the written text with an arbitrary revision.
        let (history, _state) = branching_history();
        assert!(history.to_serialized(3).is_none());
        assert!(history.to_serialized(2).is_some());
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
        let mut serialized = branching_history().0.to_serialized(0).unwrap();
        serialized.revisions.clear();
        assert!(History::from_serialized(serialized, NO_TRIM).is_err());
    }

    #[test]
    fn rejects_a_current_revision_out_of_bounds() {
        let (history, _state) = branching_history();
        let mut serialized = history.to_serialized(2).unwrap();
        serialized.current = 99;
        assert!(History::from_serialized(serialized, NO_TRIM).is_err());
    }

    #[test]
    fn rejects_a_parent_pointing_forward() {
        // `lowest_common_ancestor` walks parents until they meet and relies on
        // them strictly decreasing; a forward pointer makes it loop forever.
        let (history, _state) = branching_history();
        let mut serialized = history.to_serialized(2).unwrap();
        serialized.revisions[1].parent = 2;
        assert!(History::from_serialized(serialized, NO_TRIM).is_err());
    }

    #[test]
    fn rejects_a_last_child_out_of_bounds() {
        let (history, _state) = branching_history();
        let mut serialized = history.to_serialized(2).unwrap();
        serialized.revisions[0].last_child = Some(99);
        assert!(History::from_serialized(serialized, NO_TRIM).is_err());
    }

    #[test]
    fn rejects_a_last_child_pointing_backward() {
        // A last child must be a later revision; one pointing at an ancestor
        // (or itself) would silently corrupt `redo`, which applies
        // `revisions[last_child].transaction` unconditionally.
        let (history, _state) = branching_history();
        let mut serialized = history.to_serialized(2).unwrap();
        serialized.revisions[2].last_child = Some(0);
        assert!(History::from_serialized(serialized, NO_TRIM).is_err());
    }

    #[test]
    fn rejects_a_last_child_whose_target_disagrees_about_its_parent() {
        // Revision 1 claims revision 2 as its child, but revision 2's own
        // `parent` still points at the root: `last_child` and `parent` disagree
        // about the shape of the tree even though both are individually in
        // bounds and correctly ordered.
        let (history, _state) = branching_history();
        let mut serialized = history.to_serialized(2).unwrap();
        serialized.revisions[1].last_child = Some(2);
        assert!(History::from_serialized(serialized, NO_TRIM).is_err());
    }

    #[test]
    fn rejects_a_non_root_inversion_without_a_selection() {
        // `last_edit_pos` (history.rs) unconditionally unwraps the current
        // revision's inversion selection for any non-root current revision;
        // every legitimately committed non-root revision has one, but nothing
        // stops a corrupted file from setting it to `null`.
        let (history, _state) = branching_history();
        let mut json = serde_json::to_value(history.to_serialized(2).unwrap()).unwrap();
        json["revisions"][1]["inversion"]["selection"] = serde_json::Value::Null;
        let serialized: SerializedHistory = serde_json::from_value(json).unwrap();
        let error = History::from_serialized(serialized, NO_TRIM).unwrap_err();
        assert!(error.to_string().contains("selection"));
    }

    #[test]
    fn rejects_a_non_root_transaction_with_no_changes() {
        // `last_edit_pos` also unwraps the first item of the current revision's
        // `changes_iter()`, which yields nothing for a change set made only of
        // `Retain` operations.
        let (history, _state) = branching_history();
        let mut json = serde_json::to_value(history.to_serialized(2).unwrap()).unwrap();
        let len = json["revisions"][1]["transaction"]["changes"]["len"]
            .as_u64()
            .unwrap();
        json["revisions"][1]["transaction"]["changes"]["changes"] =
            serde_json::json!([{ "Retain": len }]);
        json["revisions"][1]["transaction"]["changes"]["len_after"] = serde_json::json!(len);
        // The inversion is adjusted to match the now-shorter transaction, so
        // this trips only the "no changes" invariant under test here and not
        // the transaction/inversion length cross-check.
        json["revisions"][1]["inversion"]["changes"]["changes"] =
            serde_json::json!([{ "Retain": len }]);
        json["revisions"][1]["inversion"]["changes"]["len"] = serde_json::json!(len);
        json["revisions"][1]["inversion"]["changes"]["len_after"] = serde_json::json!(len);
        let serialized: SerializedHistory = serde_json::from_value(json).unwrap();
        let error = History::from_serialized(serialized, NO_TRIM).unwrap_err();
        assert!(error.to_string().contains("no changes"));
    }

    #[test]
    fn rejects_a_revision_whose_inversion_length_disagrees_with_its_transaction() {
        // `Transaction::invert` guarantees an inversion's length equals its
        // transaction's length-after; swapping in another revision's
        // (individually well-formed) inversion breaks that. Revision 2's
        // inversion has length 7 here, which disagrees with revision 1's
        // transaction's length-after of 12; its length-after (6) happens to
        // still agree with revision 1's transaction's length, isolating this
        // from the other half of the same invariant.
        let (history, _state) = branching_history();
        let mut json = serde_json::to_value(history.to_serialized(2).unwrap()).unwrap();
        let other_inversion = json["revisions"][2]["inversion"].clone();
        json["revisions"][1]["inversion"] = other_inversion;
        let serialized: SerializedHistory = serde_json::from_value(json).unwrap();
        let error = History::from_serialized(serialized, NO_TRIM).unwrap_err();
        assert!(error.to_string().contains("disagrees with its transaction"));
    }

    #[test]
    fn rejects_a_revision_whose_inversion_length_after_disagrees_with_its_transaction() {
        // The other half of the same invariant: an inversion's length-after
        // must equal its transaction's length. Only `len_after` is corrupted
        // here; `len` is left agreeing with the transaction's length-after.
        let (history, _state) = branching_history();
        let mut json = serde_json::to_value(history.to_serialized(2).unwrap()).unwrap();
        json["revisions"][1]["inversion"]["changes"]["len_after"] = serde_json::json!(99);
        let serialized: SerializedHistory = serde_json::from_value(json).unwrap();
        let error = History::from_serialized(serialized, NO_TRIM).unwrap_err();
        assert!(error.to_string().contains("disagrees with its transaction"));
    }

    #[test]
    fn rejects_a_revision_whose_transaction_length_disagrees_with_its_parent() {
        // Revision 2's parent here is revision 1, not the root (unlike
        // `branching_history`, where both non-root revisions are children of
        // the root and this invariant is never exercised): its transaction's
        // required length must match what revision 1's transaction produces.
        // `inversion.len_after` is corrected alongside `transaction.len` so
        // that only the parent-continuity invariant is under test, not the
        // inversion/transaction cross-check above.
        let (history, _state) = sequential_history();
        let mut json = serde_json::to_value(history.to_serialized(2).unwrap()).unwrap();
        json["revisions"][2]["transaction"]["changes"]["len"] = serde_json::json!(99);
        json["revisions"][2]["inversion"]["changes"]["len_after"] = serde_json::json!(99);
        let serialized: SerializedHistory = serde_json::from_value(json).unwrap();
        let error = History::from_serialized(serialized, NO_TRIM).unwrap_err();
        assert!(error.to_string().contains("disagrees with its parent"));
    }

    #[test]
    fn restore_timestamps_produces_a_non_decreasing_sequence_even_when_scrambled() {
        // `jump_instant`'s binary search assumes revisions are sorted by
        // timestamp. A corrupted file need not have ordered `timestamp_unix_ms`
        // values, so the restored sequence must be non-decreasing regardless.
        let (history, _state) = branching_history();
        let mut serialized = history.to_serialized(2).unwrap();
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
