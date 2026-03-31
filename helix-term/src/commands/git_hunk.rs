use super::*;
use std::fmt::Write;
use helix_view::graphics::Rect;

pub(crate) const GIT_HUNK_PREVIEW_ID: &str = "git-hunk-preview";

fn current_diff_hunk(editor: &Editor) -> anyhow::Result<(u32, u32, Hunk, Rope, Rope, ViewId)> {
    let (view, doc) = current_ref!(editor);
    let Some(handle) = doc.diff_handle() else {
        bail!("Diff is not available in the current buffer")
    };

    let doc_text = doc.text().slice(..);
    let cursor_line = doc.selection(view.id).primary().cursor_line(doc_text) as u32;
    let diff = handle.load();
    let Some(hunk_idx) = diff.hunk_at(cursor_line, true) else {
        bail!("There is no diff change under the cursor");
    };

    Ok((
        hunk_idx,
        diff.len(),
        diff.nth_hunk(hunk_idx),
        diff.diff_base().clone(),
        diff.doc().clone(),
        view.id,
    ))
}

fn hunk_line_text(text: RopeSlice, line_idx: u32) -> String {
    text.line(line_idx as usize)
        .to_string()
        .trim_end_matches(['\n', '\r'])
        .to_owned()
}

pub(crate) struct GitHunkPopup {
    markdown: ui::Markdown,
}

impl GitHunkPopup {
    fn new(contents: String, editor: &Editor) -> Self {
        Self {
            markdown: ui::Markdown::new(contents, editor.syn_loader.clone()),
        }
    }
}

impl Component for GitHunkPopup {
    fn handle_event(
        &mut self,
        event: &compositor::Event,
        cx: &mut compositor::Context,
    ) -> compositor::EventResult {
        match event {
            compositor::Event::Key(key) if key.modifiers.is_empty() => match key.code {
                KeyCode::Char('y') => compositor::EventResult::Consumed(Some(Box::new(
                    |_compositor, cx| {
                        if let Err(err) = yank_diff_hunk(cx, Args::default(), PromptEvent::Validate)
                        {
                            cx.editor.set_error(err.to_string());
                        }
                    },
                ))),
                KeyCode::Char('o') => compositor::EventResult::Consumed(Some(Box::new(
                    |_compositor, cx| {
                        if let Err(err) = yank_diff_hunk_original_lines(
                            cx,
                            Args::default(),
                            PromptEvent::Validate,
                        ) {
                            cx.editor.set_error(err.to_string());
                        }
                    },
                ))),
                KeyCode::Char('r') => compositor::EventResult::Consumed(Some(Box::new(
                    |compositor, cx| {
                        compositor.remove(GIT_HUNK_PREVIEW_ID);
                        if let Err(err) =
                            reset_diff_hunk(cx, Args::default(), PromptEvent::Validate)
                        {
                            cx.editor.set_error(err.to_string());
                        }
                    },
                ))),
                _ => self.markdown.handle_event(event, cx),
            },
            compositor::Event::Key(_) => self.markdown.handle_event(event, cx),
            _ => self.markdown.handle_event(event, cx),
        }
    }

    fn render(
        &mut self,
        area: Rect,
        frame: &mut tui::buffer::Buffer,
        cx: &mut compositor::Context,
    ) {
        self.markdown.render(area, frame, cx);
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        self.markdown.required_size(viewport)
    }
}

fn render_diff_hunk_patch(
    hunk: Hunk,
    diff_base: RopeSlice,
    doc_text: RopeSlice,
    context_before_start: u32,
    context_after_end: u32,
) -> String {
    let mut rendered = String::new();
    let _ = writeln!(
        rendered,
        "@@ -{},{} +{},{} @@",
        hunk.before.start + 1,
        hunk.before.len(),
        hunk.after.start + 1,
        hunk.after.len()
    );

    for line_idx in context_before_start..hunk.after.start {
        let _ = writeln!(rendered, "  {}", hunk_line_text(doc_text, line_idx));
    }
    for line_idx in hunk.before.clone() {
        let _ = writeln!(rendered, "- {}", hunk_line_text(diff_base, line_idx));
    }
    for line_idx in hunk.after.clone() {
        let _ = writeln!(rendered, "+ {}", hunk_line_text(doc_text, line_idx));
    }
    for line_idx in hunk.after.end..context_after_end {
        let _ = writeln!(rendered, "  {}", hunk_line_text(doc_text, line_idx));
    }

    rendered
}

fn render_diff_hunk_markdown(
    hunk_idx: u32,
    total_hunks: u32,
    hunk: Hunk,
    diff_base: RopeSlice,
    doc_text: RopeSlice,
) -> String {
    const CONTEXT_LINES: u32 = 2;

    let removed_count = hunk.before.len();
    let added_count = hunk.after.len();
    let context_before_start = hunk.after.start.saturating_sub(CONTEXT_LINES);
    let context_after_end = (hunk.after.end + CONTEXT_LINES).min(doc_text.len_lines() as u32);

    let mut rendered = String::new();
    let _ = writeln!(rendered, "### Git Hunk {}/{}", hunk_idx + 1, total_hunks);
    let _ = writeln!(
        rendered,
        "`[g` previous, `]g` next, `y` copy hunk, `o` copy old, `r` reset hunk, `Esc` close\n"
    );
    let _ = writeln!(
        rendered,
        "removes {} line{} and adds {} line{}\n",
        removed_count,
        if removed_count == 1 { "" } else { "s" },
        added_count,
        if added_count == 1 { "" } else { "s" }
    );
    rendered.push_str("```diff\n");
    rendered.push_str(&render_diff_hunk_patch(
        hunk,
        diff_base,
        doc_text,
        context_before_start,
        context_after_end,
    ));
    rendered.push_str("```");
    rendered
}

fn git_hunk_preview_markdown(editor: &Editor) -> anyhow::Result<String> {
    let (hunk_idx, total_hunks, hunk, diff_base, doc_text, _view_id) = current_diff_hunk(editor)?;
    Ok(render_diff_hunk_markdown(
        hunk_idx,
        total_hunks,
        hunk,
        diff_base.slice(..),
        doc_text.slice(..),
    ))
}

pub(crate) fn yank_diff_hunk(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    const CONTEXT_LINES: u32 = 2;

    let (_hunk_idx, _total_hunks, hunk, diff_base, doc_text, _view_id) =
        current_diff_hunk(cx.editor)?;
    let diff_base = diff_base.slice(..);
    let doc_text = doc_text.slice(..);
    let context_before_start = hunk.after.start.saturating_sub(CONTEXT_LINES);
    let context_after_end = (hunk.after.end + CONTEXT_LINES).min(doc_text.len_lines() as u32);
    let patch = render_diff_hunk_patch(
        hunk,
        diff_base,
        doc_text,
        context_before_start,
        context_after_end,
    );

    cx.editor.registers.write('+', vec![patch])?;
    cx.editor.set_status("Copied current diff hunk to system clipboard");
    Ok(())
}

pub(crate) fn yank_diff_hunk_original_lines(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let (_hunk_idx, _total_hunks, hunk, diff_base, _doc_text, _view_id) =
        current_diff_hunk(cx.editor)?;
    anyhow::ensure!(hunk.before.len() > 0, "Current hunk has no original lines to copy");

    let mut original = String::new();
    let diff_base = diff_base.slice(..);
    for line_idx in hunk.before.clone() {
        original.push_str(&diff_base.line(line_idx as usize).to_string());
    }

    cx.editor.registers.write('+', vec![original])?;
    cx.editor
        .set_status("Copied original hunk lines to system clipboard");
    Ok(())
}

pub(crate) fn refresh_git_hunk_preview(editor: &mut Editor, compositor: &mut Compositor) {
    if let Ok(preview) = git_hunk_preview_markdown(editor) {
        let contents = GitHunkPopup::new(preview, editor);
        let popup = Popup::new(GIT_HUNK_PREVIEW_ID, contents)
            .position(editor.cursor().0)
            .position_bias(Open::Above)
            .auto_close(false);
        compositor.replace_or_push(GIT_HUNK_PREVIEW_ID, popup);
    }
}

pub(crate) fn preview_diff_hunk(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    git_hunk_preview_markdown(cx.editor)?;

    let callback = async move {
        let call: job::Callback = Callback::EditorCompositor(Box::new(
            move |editor: &mut Editor, compositor: &mut Compositor| {
                refresh_git_hunk_preview(editor, compositor);
            },
        ));
        Ok(call)
    };
    cx.jobs.callback(callback);
    cx.editor
        .set_status("Opened sticky git hunk preview. Press Esc to close.");

    Ok(())
}

pub(crate) fn reset_diff_hunk(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let scrolloff = cx.editor.config().scrolloff;
    let (_hunk_idx, _total_hunks, hunk, diff_base, doc_text, view_id) =
        current_diff_hunk(cx.editor)?;
    let (view, doc) = current!(cx.editor);
    debug_assert_eq!(view.id, view_id);

    let diff_base = diff_base.slice(..);
    let doc_text = doc_text.slice(..);
    let start = diff_base.line_to_char(hunk.before.start as usize);
    let end = diff_base.line_to_char(hunk.before.end as usize);
    let text: Tendril = diff_base.slice(start..end).chunks().collect();
    let transaction = Transaction::change(
        doc.text(),
        [(
            doc_text.line_to_char(hunk.after.start as usize),
            doc_text.line_to_char(hunk.after.end as usize),
            (!text.is_empty()).then_some(text),
        )]
        .into_iter(),
    );

    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
    view.ensure_cursor_in_view(doc, scrolloff);
    cx.editor.set_status("Reset current diff hunk");
    Ok(())
}
