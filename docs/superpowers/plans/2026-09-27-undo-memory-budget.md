# Undo Memory Budget Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound how much undo history persistent undo loads into memory when a document is opened, through a new `[editor.persistent-undo] max-memory-kib` setting.

**Architecture:** The trim happens at load time, inside `History::from_serialized`, on the plain-data form before any `Transaction` is built. Because no view has synced with the document yet and `last_saved_revision` has not been set, no index is held by anything outside `History` at that moment, so revisions can be renumbered freely — which is why `History` itself needs no changes at all. Every line of this feature lands in files this fork created.

**Tech Stack:** Rust, serde, the existing persistent-undo modules.

**Spec:** `docs/superpowers/specs/2026-09-27-undo-memory-budget-design.md`

## Global Constraints

- No upstream file is modified. All changes land in `helix-core/src/transaction/persist.rs`, `helix-core/src/history/persist.rs`, `helix-view/src/persistent_undo.rs`, `helix-term/tests/test/persistent_undo.rs` and `book/src/editor.md`. `helix-view/src/editor.rs` is NOT touched: `PersistentUndoConfig` already exists and only gains a field.
- `History` (`helix-core/src/history.rs`) must not be modified. No watermark, no guards in `undo`/`redo`/`changes_since`.
- The on-disk format does not change and `FORMAT_VERSION` stays `1`.
- Default `max-memory-kib = 32768` (32 MiB), per document. There is no unlimited setting.
- The kept set is always a subtree: a revision's `inversion` reconstructs its parent's text, so keeping a child without its parent would make undo produce a state that never existed.
- The trim runs **after** the existing graph validation and **before** the `Transaction`-building loop. Validating first means the trim can trust `parent < index`; trimming first means only kept revisions are ever materialized.
- Byte cost is the text carried in a revision's `transaction` and `inversion`. Structural overhead is not counted.
- Every failure path logs and returns; none may panic.
- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` must be clean.

## Review Focus

1. `max-memory-kib = 0`: the trim must still yield a valid one-revision history, never an empty `revisions` vector, which `from_serialized` rejects.
2. `current` is not the newest revision (the user undid, then reopened): the trim must keep `current` valid and leave its redo descendants reachable.
3. A trimmed history that is saved and loaded again must validate and must not shrink further on each cycle — the new root is index 0 and exempt from the non-root checks.
4. An undo file at the 4×-budget guard boundary: just under must load, just over must be discarded unread.
5. An existing config with no `max-memory-kib` key must keep working and get the default, rather than failing to deserialize.

---

### Task 1: Measure a serialized transaction's text

**Files:**
- Modify: `helix-core/src/transaction/persist.rs`
- Test: `helix-core/src/transaction/persist.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `impl SerializedTransaction { pub(crate) fn text_bytes(&self) -> usize }` — the total UTF-8 byte length of every `Insert` operation.
  - `impl SerializedTransaction { pub(crate) fn empty() -> Self }` — a transaction with no operations and no selection, used by Task 2 for the new root.

- [ ] **Step 1: Write the failing test**

Add to the existing `mod tests` in `helix-core/src/transaction/persist.rs`:

```rust
    #[test]
    fn measures_only_inserted_text() {
        let doc = Rope::from("hello world\n");
        // Retain 5, insert ", cruel" (7 bytes), retain 6, delete the newline.
        let transaction = Transaction::change(
            &doc,
            [(5, 5, Some(", cruel".into())), (11, 12, None)].into_iter(),
        );

        // Retains and deletes carry no text of their own, so only the insert counts.
        assert_eq!(SerializedTransaction::from(&transaction).text_bytes(), 7);
    }

    #[test]
    fn measures_multibyte_text_in_bytes_not_characters() {
        let doc = Rope::from("x");
        // "привет" is 6 characters but 12 bytes.
        let transaction = Transaction::change(&doc, [(0, 0, Some("привет".into()))].into_iter());

        assert_eq!(SerializedTransaction::from(&transaction).text_bytes(), 12);
    }

    #[test]
    fn an_empty_transaction_measures_zero_and_round_trips() {
        let empty = SerializedTransaction::empty();
        assert_eq!(empty.text_bytes(), 0);
        assert_eq!(empty.change_set_lengths(), (0, 0));

        // The new root built by the trim must survive the same path any other
        // revision takes, including deserialization of a file it was written to.
        let json = serde_json::to_string(&empty).unwrap();
        let restored: SerializedTransaction = serde_json::from_str(&json).unwrap();
        let transaction = restored.into_transaction().unwrap();

        assert!(transaction.changes_iter().next().is_none());
        assert!(transaction.selection().is_none());
    }
```

Note `ChangeSet` has no `len_chars` method — the `len_chars` in `transaction.rs` belongs to `Operation`. Use `change_set_lengths()`, which this module already provides, for length assertions.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p helix-core --lib transaction::persist`
Expected: FAIL — `no method named text_bytes`, `no function or associated item named empty`.

- [ ] **Step 3: Write the implementation**

In `helix-core/src/transaction/persist.rs`, add to the existing `impl SerializedTransaction` block, next to `change_set_lengths`:

```rust
    /// The total byte length of the text this transaction carries.
    ///
    /// Only `Insert` operations hold text; `Retain` and `Delete` are counts.
    /// This is the figure the load-time memory budget is measured against,
    /// because inserted and deleted strings are what actually occupy memory —
    /// the surrounding structures are roughly two hundred bytes against
    /// payloads measured in kilobytes.
    pub(crate) fn text_bytes(&self) -> usize {
        self.changes.text_bytes()
    }

    /// A transaction with no operations and no selection.
    ///
    /// The trim gives this to the revision it promotes to root: there is
    /// nothing above a root to undo into, so it carries no change of its own.
    /// It matches what `History::default` builds for a fresh history's root.
    pub(crate) fn empty() -> Self {
        Self {
            changes: SerializedChangeSet::empty(),
            selection: None,
        }
    }
```

Add to `impl SerializedChangeSet`:

```rust
    fn text_bytes(&self) -> usize {
        self.changes
            .iter()
            .map(|operation| match operation {
                SerializedOperation::Retain(_) | SerializedOperation::Delete(_) => 0,
                SerializedOperation::Insert(text) => text.len(),
            })
            .sum()
    }

    fn empty() -> Self {
        Self {
            changes: Vec::new(),
            len: 0,
            len_after: 0,
        }
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p helix-core --lib transaction::persist`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add helix-core/src/transaction/persist.rs
git commit -m "feat(core): measure a serialized transaction's text payload"
```

---

### Task 2: Trim the history at load time

**Files:**
- Modify: `helix-core/src/history/persist.rs`
- Test: `helix-core/src/history/persist.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `SerializedTransaction::text_bytes()` and `SerializedTransaction::empty()` (Task 1).
- Produces:
  - `History::from_serialized(serialized: SerializedHistory, max_bytes: usize) -> Result<History, InvalidHistory>` — the existing function gains a second parameter. Every existing call site and test must pass one.

This task changes a signature that `helix-view` calls. That call site is updated in Task 3; between the two tasks `cargo check -p helix-view` will fail, which is expected. Run `cargo test -p helix-core` for this task, not the workspace suite.

- [ ] **Step 1: Write the failing test**

Add to the existing `mod tests` in `helix-core/src/history/persist.rs`. `NO_TRIM` is a budget large enough that nothing is ever dropped; use it to keep the existing tests' behavior unchanged.

```rust
    /// A budget no test history can reach, for tests about something else.
    const NO_TRIM: usize = usize::MAX;

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
            let transaction =
                Transaction::change(&state.doc, [(0, state.doc.len_chars(), Some(text.into()))].into_iter())
                    .with_selection(Selection::point(0));
            history.commit_revision(&transaction, &state);
            transaction.apply(&mut state.doc);
        }
        (history, state)
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
        // Room for roughly one revision's payload: the trim must walk up from
        // `current` and stop early.
        let restored =
            History::from_serialized(history.to_serialized(3).unwrap(), 16).unwrap();

        assert!(restored.revisions.len() < 4);
        // `current` always survives, renumbered to the end of what was kept.
        assert_eq!(restored.current_revision(), restored.revisions.len() - 1);
    }

    #[test]
    fn the_promoted_root_carries_no_change() {
        let (history, _state) = linear_history();
        let restored =
            History::from_serialized(history.to_serialized(3).unwrap(), 16).unwrap();

        let root = &restored.revisions[0];
        assert_eq!(root.parent, 0);
        assert!(root.transaction.changes_iter().next().is_none());
        assert!(root.inversion.changes_iter().next().is_none());
    }

    #[test]
    fn a_trimmed_history_still_undoes_and_stops_at_its_root() {
        let (history, state) = linear_history();
        let mut restored =
            History::from_serialized(history.to_serialized(3).unwrap(), 16).unwrap();

        let mut doc = state.doc.clone();
        // Undo back to the new root, then confirm it refuses to go further
        // rather than producing text that was never on screen.
        while let Some(transaction) = restored.undo() {
            let transaction = transaction.clone();
            assert!(transaction.apply(&mut doc));
        }
        assert_eq!(restored.current_revision(), 0);
        assert!(restored.undo().is_none());
    }

    #[test]
    fn a_zero_budget_keeps_exactly_one_revision() {
        let (history, _state) = linear_history();
        let restored = History::from_serialized(history.to_serialized(3).unwrap(), 0).unwrap();

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

        let twice = History::from_serialized(
            once.to_serialized(once_current).unwrap(),
            16,
        )
        .unwrap();

        assert_eq!(twice.revisions.len(), once_len);
        assert_eq!(twice.current_revision(), once_current);
    }
```

Update every existing call of `History::from_serialized(...)` in this module's tests to pass `NO_TRIM` as the second argument, so those tests keep testing what they tested before.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p helix-core --lib history::persist`
Expected: FAIL — `this function takes 1 argument but 2 arguments were supplied`.

- [ ] **Step 3: Write the implementation**

Change the signature and insert the trim between the validation loop and the build loop in `helix-core/src/history/persist.rs`:

```rust
    pub fn from_serialized(
        mut serialized: SerializedHistory,
        max_bytes: usize,
    ) -> Result<Self, InvalidHistory> {
```

Immediately after the validation `for` loop ends and before `let timestamps = restore_timestamps(...)`, add:

```rust
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
```

Note the existing `let len = serialized.revisions.len();` near the top of the function is used by the validation loop; leave it, and add this second binding after the trim so the build loop sizes itself from the trimmed length.

Add these functions at module level:

> **Corrected after implementation, not rewritten here — this block is a record
> of what was planned.** The shedding loop below drops descendants "newest
> first" and does not distinguish abandoned branches from the redo chain. The
> shipped version instead sheds abandoned branches (off the redo chain) first,
> highest index first, and only cuts into the redo chain itself once none
> remain, deepest step first — dropping by raw index alone would shed the
> actual redo target (which `last_child` always gives the highest index among
> `current`'s descendants) before an older, permanently abandoned branch. See
> `helix-core/src/history/persist.rs` for what actually shipped, and its two
> tests pinning this order:
> `shedding_drops_abandoned_branches_before_the_redo_chain` and
> `trimming_that_reaches_into_the_redo_chain_drops_the_deepest_step_first`.

```rust
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
    // subtree overflows, shed them newest first: the highest kept index in a
    // subtree is always a leaf, because children have larger indices than their
    // parents.
    let mut current_subtree = subtree[current];
    while current_subtree > max_bytes {
        let leaf = (current + 1..len).rev().find(|&index| {
            kept[index] && is_descendant_of(&serialized.revisions, index, current)
        });
        match leaf {
            Some(index) => {
                kept[index] = false;
                current_subtree -= own[index];
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

    // Everything outside the chosen root's subtree goes, including abandoned
    // branches that are older than it.
    for index in 0..len {
        if !is_descendant_of(&serialized.revisions, index, root) && index != root {
            kept[index] = false;
        }
    }
    kept[root] = true;

    if kept.iter().all(|&keep| keep) && root == 0 {
        return;
    }

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
```

Import `SerializedTransaction` if the module does not already have it in scope — it does, via `use crate::transaction::persist::SerializedTransaction;`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p helix-core --lib history::persist`
Expected: PASS.

- [ ] **Step 5: Run the whole helix-core suite**

Run: `cargo test -p helix-core`
Expected: PASS. `cargo test --workspace` will not build until Task 3 updates helix-view's call site.

- [ ] **Step 6: Commit**

```bash
git add helix-core/src/history/persist.rs
git commit -m "feat(core): trim restored undo history to a memory budget"
```

---

### Task 3: Configure and apply the budget

**Files:**
- Modify: `helix-view/src/persistent_undo.rs`
- Test: `helix-view/src/persistent_undo.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `History::from_serialized(serialized, max_bytes)` (Task 2).
- Produces:
  - `PersistentUndoConfig { pub enable: bool, pub dir: Option<PathBuf>, pub max_memory_kib: usize }`, default `32768`.
  - The `read` path passes `config.max_memory_kib * 1024` and refuses files larger than four times that.

- [ ] **Step 1: Write the failing test**

Add to the existing `mod tests` in `helix-view/src/persistent_undo.rs`:

```rust
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

        write(&config, path, &text, history().to_serialized(1).unwrap());
        let stored = undo_file(&config, path);
        std::fs::write(&stored, "x".repeat(5 * 1024)).unwrap();

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
```

The `config` and `history` helpers already exist in that test module; bind the result as `let mut config = ...` where the test mutates it.

`toml` is already a helix-view dependency (`helix-view/Cargo.toml:57`), so no manifest change is needed and none should be made.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p helix-view --lib persistent_undo`
Expected: FAIL — `no field max_memory_kib on type PersistentUndoConfig`.

- [ ] **Step 3: Write the implementation**

In `helix-view/src/persistent_undo.rs`, add the field to `PersistentUndoConfig`:

```rust
    /// How much undo history to load into memory per document, in kibibytes.
    /// Defaults to 32768 (32 MiB).
    ///
    /// Older history beyond this is dropped at load time. There is no unlimited
    /// setting: unlimited is the behavior this budget exists to correct, since
    /// persistent undo otherwise reloads a document's entire accumulated
    /// history on every open.
    pub max_memory_kib: usize,
```

`PersistentUndoConfig` currently derives `Default`, which would give `0`. Replace the derive with an explicit implementation:

```rust
impl Default for PersistentUndoConfig {
    fn default() -> Self {
        Self {
            enable: false,
            dir: None,
            max_memory_kib: 32768,
        }
    }
}
```

Remove `Default` from the struct's `derive` list, leaving the rest as it is.

In `read`, before the file is loaded, add the guard and pass the budget through:

```rust
    let max_bytes = config.max_memory_kib.saturating_mul(1024);

    // A file this large can only predate the budget or be corrupt. `serde_json`
    // materializes the whole document before the trim can run, so reading one
    // risks an out-of-memory at open; refusing is the safer failure.
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
```

Then change the `History::from_serialized` call in the same function to pass `max_bytes`:

```rust
    match History::from_serialized(undo_file.history, max_bytes) {
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p helix-view --lib persistent_undo`
Expected: PASS.

- [ ] **Step 5: Run the workspace suite**

Run: `cargo test --workspace`
Expected: PASS. This is the first point at which the whole workspace builds again.

- [ ] **Step 6: Check formatting and lints**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: no output from fmt, no warnings from clippy.

- [ ] **Step 7: Commit**

```bash
git add helix-view/src/persistent_undo.rs helix-view/Cargo.toml
git commit -m "feat(view): bound how much undo history is loaded per document"
```

---

### Task 4: Prove it end to end and document it

**Files:**
- Modify: `helix-term/tests/test/persistent_undo.rs`
- Modify: `book/src/editor.md`

**Interfaces:**
- Consumes: `PersistentUndoConfig::max_memory_kib` (Task 3).
- Produces: nothing other tasks rely on.

- [ ] **Step 1: Write the failing test**

Add to `helix-term/tests/test/persistent_undo.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn test_a_tiny_budget_trims_history_but_keeps_undo_working() -> anyhow::Result<()> {
    let undo_dir = tempfile::tempdir()?;
    let file = helpers::temp_file_with_contents("hello\n")?;

    let mut config = persistent_undo_config(undo_dir.path());
    // One kibibyte: several of the edits below will not fit.
    config.editor.persistent_undo.max_memory_kib = 1;

    let mut app = helpers::AppBuilder::new()
        .with_config(config.clone())
        .with_file(file.path(), None)
        .build()?;
    // Four separate revisions, each a large enough insert to matter against a
    // 1 KiB budget.
    let big = "z".repeat(400);
    let keys = format!("A{big}<esc>A{big}<esc>A{big}<esc>A{big}<esc>:w<ret>");
    test_key_sequence(&mut app, Some(&keys), None, false).await?;

    let mut app = helpers::AppBuilder::new()
        .with_config(config)
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(
        &mut app,
        Some("uuuuuuuu"),
        Some(&|app: &Application| {
            let doc = doc!(app.editor);
            // Undo walks back as far as the trimmed history allows and then
            // stops. It must not panic, and it must not leave the buffer in a
            // state the document was never in: every reachable state is a
            // prefix-count of the appended blocks.
            let text = doc.text().to_string();
            assert!(
                text.starts_with("hello"),
                "unexpected buffer contents after undo: {text:?}"
            );
            let appended = text.len() - "hello\n".len();
            assert_eq!(appended % 400, 0, "buffer is not at a revision boundary");
        }),
        false,
    )
    .await?;

    Ok(())
}
```

`persistent_undo_config` already exists in that file and returns a `helix_term::config::Config`, which derives `Clone` (`helix-term/src/config.rs:12`), so the `config.clone()` above compiles as written.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo integration-test persistent_undo`
Expected: FAIL — `no field max_memory_kib`, until Task 3 is present. If Task 3 is already merged, this test should pass on first run; confirm it does and say so in your report, since a test that never failed proves less.

- [ ] **Step 3: Confirm the test passes**

Run: `cargo integration-test persistent_undo`
Expected: PASS, all persistent undo tests.

- [ ] **Step 4: Document the setting**

In `book/src/editor.md`, add a row to the `[editor.persistent-undo]` table, padding every column to its widest cell exactly as the neighbouring tables do:

```
| `max-memory-kib` | How much undo history to load into memory per document, in kibibytes | `32768` |
```

Then rewrite the paragraph that currently warns about unbounded growth. It must now say that a document's undo file converges to the budget rather than growing without limit, because each session loads at most the budget and saves back what it loaded plus its own edits. Keep the separate retention point — that undo files are never removed when their document is deleted or renamed — as it is, since that remains true.

- [ ] **Step 5: Verify the documentation renders**

Run: `mdbook build book`
Expected: the pre-existing `Helper not found fa` template error and nothing new. If a different error appears, the change broke something. If `mdbook` is unavailable, read the section and confirm the table and fences are intact.

- [ ] **Step 6: Commit**

```bash
git add helix-term/tests/test/persistent_undo.rs book/src/editor.md
git commit -m "test: cover the undo memory budget end to end, and document it"
```
