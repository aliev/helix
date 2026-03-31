use super::*;
use std::fmt::Write;

use helix_core::history::RevisionInfo;
use helix_core::{Assoc, Rope};

pub(crate) fn earlier(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let uk = args.join(" ").parse::<UndoKind>().map_err(|s| anyhow!(s))?;

    let (view, doc) = current!(cx.editor);
    let success = doc.earlier(view, uk);
    if !success {
        cx.editor.set_status("Already at oldest change");
    }

    Ok(())
}

pub(crate) fn later(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let uk = args.join(" ").parse::<UndoKind>().map_err(|s| anyhow!(s))?;
    let (view, doc) = current!(cx.editor);
    let success = doc.later(view, uk);
    if !success {
        cx.editor.set_status("Already at newest change");
    }

    Ok(())
}

#[derive(Clone)]
struct UndoListItem {
    revision: usize,
    parent: usize,
    summary: String,
    is_current: bool,
}

#[derive(Clone, Copy)]
struct UndoListData {
    doc_id: DocumentId,
}

fn summarize_undo_revision(
    revision: RevisionInfo<'_>,
    current_text: &Rope,
    root_text: &Rope,
) -> String {
    if revision.id == 0 {
        return format!("session start ({})", root_text.len_lines().saturating_sub(1));
    }

    let mut inserts = 0usize;
    let mut deletes = 0usize;
    let mut fragments = Vec::new();
    for (from, to, fragment) in revision.transaction.changes_iter() {
        deletes += to.saturating_sub(from);
        if let Some(fragment) = fragment {
            inserts += fragment.chars().count();
            let snippet = fragment.lines().next().unwrap_or_default().trim();
            if !snippet.is_empty() {
                fragments.push(snippet.chars().take(24).collect::<String>());
            }
        }
    }

    let mut parts = Vec::new();
    if inserts > 0 {
        parts.push(format!("+{inserts}"));
    }
    if deletes > 0 {
        parts.push(format!("-{deletes}"));
    }
    if parts.is_empty() {
        parts.push("selection".to_string());
    }

    let context = if fragments.is_empty() {
        current_text
            .len_lines()
            .checked_sub(1)
            .map(|_| "".to_string())
            .unwrap_or_default()
    } else {
        format!(" {}", fragments.join(" | "))
    };

    format!("{}{}", parts.join(" "), context)
}

fn render_undo_preview(parent: Option<&Rope>, current: &Rope) -> String {
    fn collect_changed_lines(text: &Rope, start: usize, end: usize) -> Vec<String> {
        if text.len_chars() == 0 {
            return Vec::new();
        }

        let last_char = text.len_chars().saturating_sub(1);
        let start = start.min(text.len_chars());
        let start_line = text.char_to_line(start.min(last_char));
        let end_anchor = if end > start {
            end.saturating_sub(1).min(last_char)
        } else {
            start.min(last_char)
        };
        let end_line = text.char_to_line(end_anchor);

        (start_line..=end_line)
            .map(|line| {
                text.line(line)
                    .to_string()
                    .trim_end_matches(['\r', '\n'])
                    .to_string()
            })
            .collect()
    }

    let mut out = String::new();
    match parent {
        None => {
            let _ = writeln!(out, "Session start");
            let _ = writeln!(out);
            for line in current.lines() {
                let _ = writeln!(out, "  {}", line);
            }
        }
        Some(parent) => {
            let transaction = helix_core::diff::compare_ropes(parent, current);
            let mut has_changes = false;
            for (from, to, fragment) in transaction.changes_iter() {
                has_changes = true;
                let removed_lines = if from == to {
                    Vec::new()
                } else {
                    collect_changed_lines(parent, from, to)
                };
                let mapped_from = transaction.changes().map_pos(from, Assoc::Before);
                let mapped_to = transaction.changes().map_pos(to, Assoc::After);
                let inserted_lines = if fragment.is_some() || mapped_from != mapped_to {
                    collect_changed_lines(current, mapped_from, mapped_to)
                } else {
                    Vec::new()
                };

                if removed_lines.is_empty() && inserted_lines.is_empty() {
                    let _ = writeln!(out, "(selection change)");
                } else {
                    for line in removed_lines {
                        let _ = writeln!(out, "- {}", line);
                    }
                    for line in inserted_lines {
                        let _ = writeln!(out, "+ {}", line);
                    }
                }
                let _ = writeln!(out);
            }
            if !has_changes {
                let _ = writeln!(out, "No text changes in this revision.");
            }
        }
    }

    out
}

fn build_undo_preview_document(
    editor: &Editor,
    data: UndoListData,
    item: &UndoListItem,
) -> Option<Box<helix_view::Document>> {
    let doc = editor.document(data.doc_id)?;
    let current_text = doc.text().clone();
    let (parent_text, revision_text) = doc.with_history(|history| {
        let revision_text = history.snapshot_at(&current_text, item.revision)?;
        let parent_text = if item.revision == 0 {
            None
        } else {
            history.snapshot_at(&current_text, item.parent)
        };
        Some((parent_text, revision_text))
    })?;

    let preview = render_undo_preview(parent_text.as_ref(), &revision_text);
    Some(Box::new(helix_view::Document::from(
        Rope::from(preview),
        None,
        editor.config.clone(),
        editor.syn_loader.clone(),
    )))
}

pub(crate) fn undo_list(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let (view, doc) = current!(cx.editor);
    doc.append_changes_to_history(view);

    let current_text = doc.text().clone();
    let root_text = doc.with_history(|history| history.root_doc(&current_text));
    let items: Vec<_> = doc.with_history(|history| {
        history
            .revisions()
            .filter(|revision| revision.id != 0)
            .map(|revision| UndoListItem {
                revision: revision.id,
                parent: revision.parent,
                summary: summarize_undo_revision(revision, &current_text, &root_text),
                is_current: revision.is_current,
            })
            .collect::<Vec<_>>()
    });

    if items.is_empty() {
        cx.editor.set_status("No undo history");
        return Ok(());
    }

    let initial_cursor = items.iter().position(|item| item.is_current).unwrap_or(0) as u32;
    let data = UndoListData { doc_id: doc.id() };
    let columns = [
        ui::PickerColumn::new("rev", |item: &UndoListItem, _| item.revision.to_string().into()),
        ui::PickerColumn::new("parent", |item: &UndoListItem, _| item.parent.to_string().into()),
        ui::PickerColumn::new("flags", |item: &UndoListItem, _| {
            if item.is_current { "*".into() } else { "".into() }
        }),
        ui::PickerColumn::new("change", |item: &UndoListItem, _| item.summary.as_str().into()),
    ];

    let callback = async move {
        let call: job::Callback = job::Callback::EditorCompositor(Box::new(
            move |_editor, compositor| {
                let preview_data = data;
                let picker = ui::Picker::new(columns, 3, items, data, |cx, item, _action| {
                    let (view, doc) = current!(cx.editor);
                    if !doc.jump_to_history_revision(view, item.revision) {
                        cx.editor.set_error("failed to jump to history revision");
                    }
                })
                .with_initial_cursor(initial_cursor)
                .with_preview_document(move |editor, item| {
                    build_undo_preview_document(editor, preview_data, item).map(|doc| (doc, None))
                });
                compositor.push(Box::new(overlaid(picker)));
            },
        ));
        Ok(call)
    };
    cx.jobs.callback(callback);
    Ok(())
}
