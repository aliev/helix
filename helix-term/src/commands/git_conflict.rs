use super::*;
use helix_core::{movement::Direction, RopeSlice, Selection, Tendril, Transaction};
use helix_view::graphics::{Margin, Rect};
use helix_view::input::KeyModifiers;
use helix_view::theme::Modifier;
use tui::text::{Span, Spans, Text as TuiText};

pub(crate) const GIT_CONFLICT_PREVIEW_ID: &str = "git-conflict-preview";
const CONFLICT_START_MARKER: &str = "<<<<<<<";
const CONFLICT_BASE_MARKER: &str = "|||||||";
const CONFLICT_SEPARATOR_MARKER: &str = "=======";
const CONFLICT_END_MARKER: &str = ">>>>>>>";

#[derive(Debug, Clone, Copy)]
struct ConflictBlock {
    start_line: usize,
    base_line: Option<usize>,
    separator_line: usize,
    end_line: usize,
}

impl ConflictBlock {
    fn contains_line(self, line: usize) -> bool {
        (self.start_line..=self.end_line).contains(&line)
    }

    fn range(self, text: RopeSlice) -> std::ops::Range<usize> {
        text.line_to_char(self.start_line)
            ..text.line_to_char((self.end_line + 1).min(text.len_lines()))
    }

    fn ours_line_range(self) -> std::ops::Range<usize> {
        let end = self.base_line.unwrap_or(self.separator_line);
        (self.start_line + 1)..end
    }

    fn theirs_line_range(self) -> std::ops::Range<usize> {
        (self.separator_line + 1)..self.end_line
    }
}

fn line_trimmed_text(text: RopeSlice, line_idx: usize) -> String {
    text.line(line_idx)
        .to_string()
        .trim_end_matches(['\n', '\r'])
        .to_owned()
}

fn is_conflict_start_line(text: RopeSlice, line_idx: usize) -> bool {
    let line = line_trimmed_text(text, line_idx);
    line == CONFLICT_START_MARKER
        || line
            .strip_prefix(CONFLICT_START_MARKER)
            .is_some_and(|rest| rest.starts_with(' '))
}

fn is_conflict_base_line(text: RopeSlice, line_idx: usize) -> bool {
    let line = line_trimmed_text(text, line_idx);
    line == CONFLICT_BASE_MARKER
        || line
            .strip_prefix(CONFLICT_BASE_MARKER)
            .is_some_and(|rest| rest.starts_with(' '))
}

fn is_conflict_separator_line(text: RopeSlice, line_idx: usize) -> bool {
    line_trimmed_text(text, line_idx) == CONFLICT_SEPARATOR_MARKER
}

fn is_conflict_end_line(text: RopeSlice, line_idx: usize) -> bool {
    let line = line_trimmed_text(text, line_idx);
    line == CONFLICT_END_MARKER
        || line
            .strip_prefix(CONFLICT_END_MARKER)
            .is_some_and(|rest| rest.starts_with(' '))
}

fn conflict_side_label(text: RopeSlice, line_idx: usize, marker: &str, fallback: &str) -> String {
    let line = line_trimmed_text(text, line_idx);
    line.strip_prefix(marker)
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

fn parse_conflict_blocks(text: RopeSlice) -> Vec<ConflictBlock> {
    let mut conflicts = Vec::new();
    let mut line = 0;

    while line < text.len_lines() {
        if !is_conflict_start_line(text, line) {
            line += 1;
            continue;
        }

        let start_line = line;
        line += 1;

        let mut base_line = None;
        while line < text.len_lines()
            && !is_conflict_base_line(text, line)
            && !is_conflict_separator_line(text, line)
        {
            line += 1;
        }
        if line >= text.len_lines() {
            break;
        }

        if is_conflict_base_line(text, line) {
            base_line = Some(line);
            line += 1;
            while line < text.len_lines() && !is_conflict_separator_line(text, line) {
                line += 1;
            }
            if line >= text.len_lines() {
                break;
            }
        }

        let separator_line = line;
        line += 1;
        while line < text.len_lines() && !is_conflict_end_line(text, line) {
            line += 1;
        }
        if line >= text.len_lines() {
            break;
        }

        conflicts.push(ConflictBlock {
            start_line,
            base_line,
            separator_line,
            end_line: line,
        });
        line += 1;
    }

    conflicts
}

fn current_conflict(editor: &Editor) -> anyhow::Result<(ConflictBlock, usize, usize)> {
    let (view, doc) = current_ref!(editor);
    let text = doc.text().slice(..);
    let cursor_line = doc.selection(view.id).primary().cursor_line(text);
    let conflicts = parse_conflict_blocks(text);
    let Some((idx, conflict)) = conflicts
        .iter()
        .copied()
        .enumerate()
        .find(|(_, conflict)| conflict.contains_line(cursor_line))
    else {
        bail!("There is no merge conflict under the cursor");
    };

    Ok((conflict, idx, conflicts.len()))
}

fn conflict_section_text(text: RopeSlice, lines: std::ops::Range<usize>) -> Tendril {
    let start = text.line_to_char(lines.start);
    let end = text.line_to_char(lines.end.min(text.len_lines()));
    text.slice(start..end).chunks().collect()
}

pub(crate) struct GitConflictPopup {
    text: TuiText<'static>,
}

impl GitConflictPopup {
    fn new(
        conflict: ConflictBlock,
        idx: usize,
        total: usize,
        editor: &Editor,
        text: RopeSlice,
    ) -> Self {
        let ours = conflict_section_text(text, conflict.ours_line_range());
        let theirs = conflict_section_text(text, conflict.theirs_line_range());
        let ours_label =
            conflict_side_label(text, conflict.start_line, CONFLICT_START_MARKER, "current branch");
        let theirs_label =
            conflict_side_label(text, conflict.end_line, CONFLICT_END_MARKER, "incoming branch");

        let base_style = editor.theme.get("ui.text");
        let title_style = base_style.add_modifier(Modifier::BOLD);
        let hint_style = editor.theme.get("ui.virtual");
        let separator_style = hint_style;
        let ours_style = editor
            .theme
            .find_highlight_exact("diff.minus")
            .map(|highlight| editor.theme.highlight(highlight))
            .unwrap_or(base_style)
            .add_modifier(Modifier::BOLD);
        let theirs_style = editor
            .theme
            .find_highlight_exact("diff.plus")
            .map(|highlight| editor.theme.highlight(highlight))
            .unwrap_or(base_style)
            .add_modifier(Modifier::BOLD);
        let section_label_style = hint_style;
        let shortcut_style = hint_style.add_modifier(Modifier::BOLD);
        let separator = "─".repeat(44);
        let line_range = format!("L{}-L{}", conflict.start_line + 1, conflict.end_line + 1);

        let mut lines = vec![
            Spans::from(vec![
                Span::styled(format!("Conflict {}/{}", idx + 1, total), title_style),
                Span::raw("   "),
                Span::styled(line_range, hint_style),
                Span::raw("   "),
                Span::styled("n", shortcut_style),
                Span::styled(" next", hint_style),
                Span::raw("  "),
                Span::styled("N", shortcut_style),
                Span::styled(" prev", hint_style),
                Span::raw("  "),
                Span::styled("o", shortcut_style),
                Span::styled(" ours", hint_style),
                Span::raw("  "),
                Span::styled("t", shortcut_style),
                Span::styled(" theirs", hint_style),
                Span::raw("  "),
                Span::styled("b", shortcut_style),
                Span::styled(" both", hint_style),
                Span::raw("  "),
                Span::styled("Esc", shortcut_style),
                Span::styled(" close", hint_style),
            ]),
            Spans::default(),
            Spans::from(vec![
                Span::styled("OURS", ours_style),
                Span::styled(format!("  {}", ours_label), section_label_style),
            ]),
            Spans::from(Span::styled(separator.clone(), separator_style)),
        ];
        lines.extend(
            ours.lines()
                .map(|line| Spans::from(Span::styled(line.to_string(), base_style))),
        );
        lines.push(Spans::from(vec![
            Span::styled("THEIRS", theirs_style),
            Span::styled(format!("  {}", theirs_label), section_label_style),
        ]));
        lines.push(Spans::from(Span::styled(separator, separator_style)));
        lines.extend(
            theirs
                .lines()
                .map(|line| Spans::from(Span::styled(line.to_string(), base_style))),
        );

        Self {
            text: TuiText::from(lines),
        }
    }
}

impl Component for GitConflictPopup {
    fn handle_event(
        &mut self,
        event: &compositor::Event,
        _cx: &mut compositor::Context,
    ) -> compositor::EventResult {
        match event {
            compositor::Event::Key(key) if key.modifiers.is_empty() => match key.code {
                KeyCode::Char('n') => compositor::EventResult::Consumed(Some(Box::new(
                    |compositor, cx| {
                        if let Err(err) =
                            goto_next_git_conflict(cx, Args::default(), PromptEvent::Validate)
                        {
                            cx.editor.set_error(err.to_string());
                        } else {
                            refresh_git_conflict_preview(cx.editor, compositor);
                        }
                    },
                ))),
                KeyCode::Char('N') => compositor::EventResult::Consumed(Some(Box::new(
                    |compositor, cx| {
                        if let Err(err) =
                            goto_prev_git_conflict(cx, Args::default(), PromptEvent::Validate)
                        {
                            cx.editor.set_error(err.to_string());
                        } else {
                            refresh_git_conflict_preview(cx.editor, compositor);
                        }
                    },
                ))),
                KeyCode::Char('o') => compositor::EventResult::Consumed(Some(Box::new(
                    |compositor, cx| {
                        compositor.remove(GIT_CONFLICT_PREVIEW_ID);
                        if let Err(err) =
                            resolve_git_conflict_ours(cx, Args::default(), PromptEvent::Validate)
                        {
                            cx.editor.set_error(err.to_string());
                        } else {
                            refresh_git_conflict_preview(cx.editor, compositor);
                        }
                    },
                ))),
                KeyCode::Char('t') => compositor::EventResult::Consumed(Some(Box::new(
                    |compositor, cx| {
                        compositor.remove(GIT_CONFLICT_PREVIEW_ID);
                        if let Err(err) = resolve_git_conflict_theirs(
                            cx,
                            Args::default(),
                            PromptEvent::Validate,
                        ) {
                            cx.editor.set_error(err.to_string());
                        } else {
                            refresh_git_conflict_preview(cx.editor, compositor);
                        }
                    },
                ))),
                KeyCode::Char('b') => compositor::EventResult::Consumed(Some(Box::new(
                    |compositor, cx| {
                        compositor.remove(GIT_CONFLICT_PREVIEW_ID);
                        if let Err(err) =
                            resolve_git_conflict_both(cx, Args::default(), PromptEvent::Validate)
                        {
                            cx.editor.set_error(err.to_string());
                        } else {
                            refresh_git_conflict_preview(cx.editor, compositor);
                        }
                    },
                ))),
                _ => compositor::EventResult::Ignored(None),
            },
            compositor::Event::Key(key) if key.modifiers == KeyModifiers::SHIFT => match key.code {
                KeyCode::Char('N') => compositor::EventResult::Consumed(Some(Box::new(
                    |compositor, cx| {
                        if let Err(err) =
                            goto_prev_git_conflict(cx, Args::default(), PromptEvent::Validate)
                        {
                            cx.editor.set_error(err.to_string());
                        } else {
                            refresh_git_conflict_preview(cx.editor, compositor);
                        }
                    },
                ))),
                _ => compositor::EventResult::Ignored(None),
            },
            _ => compositor::EventResult::Ignored(None),
        }
    }

    fn render(
        &mut self,
        area: Rect,
        frame: &mut tui::buffer::Buffer,
        cx: &mut compositor::Context,
    ) {
        use tui::widgets::{Paragraph, Widget, Wrap};

        let inner = area.inner(Margin::all(1));
        Paragraph::new(&self.text)
            .wrap(Wrap { trim: false })
            .scroll((cx.scroll.unwrap_or_default() as u16, 0))
            .render(inner, frame);
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        const PADDING: u16 = 2;
        const MAX_TEXT_WIDTH: u16 = 72;

        let max_text_width = viewport.0.saturating_sub(PADDING).min(MAX_TEXT_WIDTH);
        let mut width = 0;
        let mut height = 0;
        for line in &self.text.lines {
            height += 1;
            let line_width = line.width() as u16;
            if line_width > max_text_width {
                width = max_text_width;
                height += line_width.checked_div(max_text_width).unwrap_or(0);
            } else if line_width > width {
                width = line_width;
            }
        }
        Some((width + PADDING, height + PADDING))
    }
}

pub(crate) fn preview_git_conflict(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let (conflict, idx, total) = current_conflict(cx.editor)?;

    let callback = async move {
        let call: job::Callback = Callback::EditorCompositor(Box::new(
            move |editor: &mut Editor, compositor: &mut Compositor| {
                let (_view, doc) = current_ref!(editor);
                let contents =
                    GitConflictPopup::new(conflict, idx, total, editor, doc.text().slice(..));
                let popup = Popup::new(GIT_CONFLICT_PREVIEW_ID, contents)
                    .position(editor.cursor().0)
                    .position_bias(Open::Above)
                    .auto_close(false);
                compositor.replace_or_push(GIT_CONFLICT_PREVIEW_ID, popup);
            },
        ));
        Ok(call)
    };
    cx.jobs.callback(callback);

    Ok(())
}

pub(crate) fn refresh_git_conflict_preview(editor: &mut Editor, compositor: &mut Compositor) {
    let Ok((conflict, idx, total)) = current_conflict(editor) else {
        compositor.remove(GIT_CONFLICT_PREVIEW_ID);
        return;
    };

    let (_view, doc) = current_ref!(editor);
    let contents = GitConflictPopup::new(conflict, idx, total, editor, doc.text().slice(..));
    let popup = Popup::new(GIT_CONFLICT_PREVIEW_ID, contents)
        .position(editor.cursor().0)
        .position_bias(Open::Above)
        .auto_close(false);
    compositor.replace_or_push(GIT_CONFLICT_PREVIEW_ID, popup);
}

fn goto_git_conflict_impl(editor: &mut Editor, direction: Direction) -> anyhow::Result<()> {
    let scrolloff = editor.config().scrolloff;
    let (view, doc) = current!(editor);
    let text = doc.text().slice(..);
    let cursor_line = doc.selection(view.id).primary().cursor_line(text);
    let conflicts = parse_conflict_blocks(text);
    let target = match direction {
        Direction::Forward => conflicts
            .into_iter()
            .find(|conflict| conflict.start_line > cursor_line),
        Direction::Backward => conflicts
            .into_iter()
            .rev()
            .find(|conflict| conflict.end_line < cursor_line),
    };
    let Some(conflict) = target else {
        bail!(
            "{} merge conflict",
            match direction {
                Direction::Forward => "No next",
                Direction::Backward => "No previous",
            }
        );
    };

    doc.set_selection(view.id, Selection::point(text.line_to_char(conflict.start_line)));
    view.ensure_cursor_in_view(doc, scrolloff);
    Ok(())
}

pub(crate) fn goto_next_git_conflict(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    goto_git_conflict_impl(cx.editor, Direction::Forward)
}

pub(crate) fn goto_prev_git_conflict(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    goto_git_conflict_impl(cx.editor, Direction::Backward)
}

fn resolve_git_conflict(
    cx: &mut compositor::Context,
    event: PromptEvent,
    replacement_for: impl Fn(ConflictBlock, RopeSlice) -> Tendril,
    status: &str,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let scrolloff = cx.editor.config().scrolloff;
    let (conflict, idx, total) = current_conflict(cx.editor)?;
    let (view, doc) = current!(cx.editor);
    let text = doc.text().slice(..);
    let anchor_line = conflict.start_line;
    let range = conflict.range(text);
    let replacement = replacement_for(conflict, text);
    let transaction = Transaction::change(
        doc.text(),
        [(range.start, range.end, Some(replacement))].into_iter(),
    );

    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
    let updated_text = doc.text().slice(..);
    if let Some(next_conflict) = parse_conflict_blocks(updated_text)
        .into_iter()
        .find(|conflict| conflict.start_line >= anchor_line)
    {
        doc.set_selection(
            view.id,
            Selection::point(updated_text.line_to_char(next_conflict.start_line)),
        );
    }
    view.ensure_cursor_in_view(doc, scrolloff);
    cx.editor
        .set_status(format!("{status} for conflict {}/{}", idx + 1, total));
    Ok(())
}

pub(crate) fn resolve_git_conflict_ours(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    resolve_git_conflict(
        cx,
        event,
        |conflict, text| conflict_section_text(text, conflict.ours_line_range()),
        "Accepted ours",
    )
}

pub(crate) fn resolve_git_conflict_theirs(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    resolve_git_conflict(
        cx,
        event,
        |conflict, text| conflict_section_text(text, conflict.theirs_line_range()),
        "Accepted theirs",
    )
}

pub(crate) fn resolve_git_conflict_both(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    resolve_git_conflict(
        cx,
        event,
        |conflict, text| {
            let mut combined = conflict_section_text(text, conflict.ours_line_range());
            combined.push_str(&conflict_section_text(text, conflict.theirs_line_range()));
            combined
        },
        "Accepted both",
    )
}
