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
            selection: transaction
                .selection
                .as_ref()
                .map(SerializedSelection::from),
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
        // `checked_add` rather than `+=`: the operands come straight from
        // deserialized JSON, so an overflowing sum must be rejected instead of
        // panicking (debug) or wrapping (release) and letting a corrupted
        // length claim pass by wraparound.
        let overflow = || InvalidHistory::new("change set length overflows");
        let mut len: usize = 0;
        let mut len_after: usize = 0;
        for operation in &changes {
            match operation {
                Operation::Retain(n) => {
                    len = len.checked_add(*n).ok_or_else(overflow)?;
                    len_after = len_after.checked_add(*n).ok_or_else(overflow)?;
                }
                Operation::Delete(n) => len = len.checked_add(*n).ok_or_else(overflow)?,
                Operation::Insert(text) => {
                    len_after = len_after
                        .checked_add(text.chars().count())
                        .ok_or_else(overflow)?
                }
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
            return Err(InvalidHistory::new(
                "selection primary index is out of bounds",
            ));
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
        let json =
            r#"{"changes":{"changes":[{"Retain":5}],"len":99,"len_after":5},"selection":null}"#;
        let error = serde_json::from_str::<SerializedTransaction>(json)
            .unwrap()
            .into_transaction()
            .unwrap_err();
        assert!(error.to_string().contains("change set length"));
    }

    #[test]
    fn rejects_a_change_set_whose_length_overflows() {
        // Two retains near `usize::MAX` overflow when summed. Trusting that
        // arithmetic would panic (debug) or silently wrap (release), letting a
        // corrupted change set's length claim pass by wraparound.
        let json = format!(
            r#"{{"changes":{{"changes":[{{"Retain":{max}}},{{"Retain":{max}}}],"len":0,"len_after":0}},"selection":null}}"#,
            max = usize::MAX,
        );
        let error = serde_json::from_str::<SerializedTransaction>(&json)
            .unwrap()
            .into_transaction()
            .unwrap_err();
        assert!(error.to_string().contains("overflow"));
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
