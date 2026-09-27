# A memory budget for restored undo history

Date: 2026-09-27
Status: approved, ready for implementation planning

## Goal

Bound how much undo history persistent undo loads into memory when a document
is opened, through a new `[editor.persistent-undo] max-memory-kib` setting.

This fixes a regression that persistent undo itself introduced. Before it,
helix's undo history was unbounded but *self-healing*: a long session could grow
it without limit, and restarting the editor cleared it. Persistent undo removed
that escape hatch — reopening a document now reloads its entire accumulated
history, and every session adds to it. What used to be a session-scoped problem
became a monotonically growing one.

Restoring the escape hatch is the whole objective: after this change, restarting
frees memory again, as it did before persistent undo existed.

## Non-goals

- **Bounding history within a session.** helix's `History` vector is unbounded
  during editing, and its own doc comment names that as a known limitation. That
  is upstream's behavior, it predates this fork, and it stays as it is. The scope
  here is exactly the part this fork broke.
- Anything resembling vim's `'undolevels'`. Despite the resemblance, this is not
  a cap on undo depth as an editor-wide property: with persistent undo disabled,
  nothing is bounded.
- Garbage collection of the undo directory.

## The key insight: renumbering is safe at load time and only at load time

An index into the revision vector is dangerous only while something is *holding*
one as the numbering changes. There are four holders, in two groups:

- Inside `History`: `current`, `Revision::parent`, `Revision::last_child`. A
  renumbering fixes these itself.
- Outside, beyond `History`'s reach: `Document::last_saved_revision` and
  `View::doc_revisions[doc_id]`. `History` lives inside a `Document` and knows
  nothing about views.

Renumbering at runtime would leave those external holders pointing at the wrong
revisions. A view would then call `changes_since(its_stale_index)` and receive a
transaction describing edits that never happened from its perspective, and apply
it to its jumplist and selections — corruption with nothing in the logs.

At load time none of those holders exist yet. The document has just been built by
`Document::open`; `last_saved_revision` is still 0 and the restore hook is about
to overwrite it; and no view has ever synced with this document, so it has no
`doc_revisions` entry. There is nothing to invalidate.

The index problem is therefore a problem about *time*, not about structure. It is
not solved with a cleverer representation — it is avoided by doing the work at
the one moment when it does not exist.

The consequence that matters for this fork: **`History` is not modified at all.**
No watermark field, no guards in `undo`, `redo` or `changes_since`, no new lines
in any upstream file. The entire feature lives in
`helix-core/src/history/persist.rs` and `helix-view/src/persistent_undo.rs`, both
created by this fork in the persistent-undo work.

## Why the kept set is a subtree, not "the newest N"

A revision's `inversion` reconstructs the text of its **parent**. Drop a parent
while keeping its child and undo produces a buffer state that never existed.

So the kept set must be closed under the parent relation: it is always a subtree
rooted at some revision R. R itself has its `transaction` and `inversion`
emptied, because there is nothing above it left to undo into.

After renumbering, R becomes index 0 — and the validation written for persistent
undo already exempts index 0 from the "a non-root revision must carry changes and
an inversion selection" rule. **No change to the on-disk format, the validation,
or the format version is required.**

## The trim

Applied in `History::from_serialized`, which already owns validation and
construction, on the plain-data form before any `Transaction` is built. The
budget reaches it as a new parameter; both that function and its only caller,
`helix-view/src/persistent_undo.rs`, belong to this fork, so the signature change
costs no upstream surface.

1. One reverse pass over the serialized revisions accumulates each revision's own
   byte cost into its subtree total. Parents always precede children, so a single
   backwards iteration is enough.
2. Walk up the parent chain from the persisted `current`, stopping at the last
   ancestor whose subtree still fits the budget — the one closest to the original
   root, which keeps the most undo depth. That ancestor is R.
3. If `current`'s own subtree does not fit, drop its descendant branches — they
   are reachable only by redo — newest first, until it does. If `current` alone
   still exceeds the budget, keep `current` alone.
4. Keep R and its descendants, renumber them from zero, empty R's payloads, and
   remap `parent`, `last_child` and `current`.

**Byte cost** is the text carried in a revision's `transaction` and `inversion` —
the inserted and deleted strings. Structural overhead is not counted: it is
roughly 200 bytes per revision against payloads measured in kilobytes, and
counting it would make the number harder to reason about without changing any
decision it drives.

## The transient peak, and why the file stays bounded

`serde_json` materializes the whole file before the trim can run, so the peak
allocation at load is the size of the file, not the budget.

This is self-correcting: the session saves the trimmed history back, so the file
converges to the budget plus one session's growth, and every later load is
bounded. The peak can only be exceeded by a file written before this change
existed.

For that case, and for a corrupted length, an undo file larger than four times
the budget is discarded unread, with a warning. Four is a safety valve chosen to
sit above JSON's overhead against the in-memory form, not a precise bound.

This also means the documentation's current warning that undo files grow without
limit stops being true and must be rewritten.

## Configuration

```toml
[editor.persistent-undo]
enable = true
max-memory-kib = 32768   # 32 MiB, per document
```

The budget is **per document**, not global across open buffers. A global budget
would mean that opening one document trims the history of others — a trim after
load, when views already hold indices, which is precisely what this design
exists to avoid. A global cap would drag back everything this approach removes.

helix writes no unit suffixes in configuration values: `idle-timeout = 250` is
milliseconds, and the unit lives only in the documentation. A bare integer is
therefore the house style; the unit sits in the key name because, unlike a
timeout in milliseconds, kibibytes are not the obvious default for a memory
budget.

Default 32768 (32 MiB). There is no unlimited setting: unlimited is the
regression being fixed.

## Edge cases

| Case | Behavior |
| --- | --- |
| History already fits the budget | Nothing is trimmed; the loaded history is identical to today's |
| Budget smaller than a single revision | That revision is kept alone; a large edit must not leave an empty history |
| `current` is the root | Nothing to walk up to; the history is kept or trimmed to `current` alone |
| Undo file larger than 4× the budget | Discarded unread with a warning, rather than risking an out-of-memory at open |
| Persistent undo disabled | The setting has no effect; nothing is loaded and nothing is bounded |
| Old abandoned branches | Dropped with everything outside R's subtree. They are unreachable by `u`/`U` and were only reachable by `:earlier` through time |

## Testing

1. **Trim selection** (`helix-core/src/history/persist.rs`): a history under
   budget is untouched; a history over budget keeps a suffix of the ancestor
   chain; the chosen root has empty payloads; `parent`, `last_child` and
   `current` are remapped consistently; a single oversized revision is kept
   alone; descendant branches are pruned when `current`'s subtree overflows.
2. **Renumbering is sound**: after a trim, undo from `current` walks to the new
   root and stops, redo returns along the kept branch, and every retained
   revision reconstructs the same text it would have before the trim.
3. **Validation still passes**: a trimmed history serializes and round-trips
   through the existing `from_serialized` checks without special-casing.
4. **File-size guard** (`helix-view/src/persistent_undo.rs`): an oversized file
   is discarded unread.
5. **Integration** (`helix-term/tests/test/persistent_undo.rs`): with a tiny
   budget, a document with a long history reopens, `u` still works, and it stops
   at the trimmed boundary rather than misbehaving.

## Documentation

`book/src/editor.md`: the new key in the `[editor.persistent-undo]` table, and a
rewrite of the growth warning — files converge to the budget rather than growing
without limit.
