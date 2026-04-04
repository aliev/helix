use super::*;
use helix_view::graphics::Rect;
use std::fmt::Write;

pub(crate) const FILE_LOCATION_POPUP_ID: &str = "file-location-popup";

pub(crate) struct FileLocationPopup {
    markdown: ui::Markdown,
}

impl FileLocationPopup {
    fn new(contents: String, editor: &Editor) -> Self {
        Self {
            markdown: ui::Markdown::new(contents, editor.syn_loader.clone()),
        }
    }
}

impl Component for FileLocationPopup {
    fn handle_event(
        &mut self,
        event: &compositor::Event,
        cx: &mut compositor::Context,
    ) -> compositor::EventResult {
        match event {
            compositor::Event::Key(key) if key.modifiers.is_empty() => match key.code {
                KeyCode::Char('y') => compositor::EventResult::Consumed(Some(Box::new(
                    |_compositor, cx| {
                        if let Err(err) = copy_relative_location(cx) {
                            cx.editor.set_error(err.to_string());
                        }
                    },
                ))),
                KeyCode::Char('a') => compositor::EventResult::Consumed(Some(Box::new(
                    |_compositor, cx| {
                        if let Err(err) = copy_absolute_location(cx) {
                            cx.editor.set_error(err.to_string());
                        }
                    },
                ))),
                KeyCode::Char('r') => compositor::EventResult::Consumed(Some(Box::new(
                    |_compositor, cx| {
                        if let Err(err) = copy_relative_path(cx) {
                            cx.editor.set_error(err.to_string());
                        }
                    },
                ))),
                KeyCode::Char('f') => compositor::EventResult::Consumed(Some(Box::new(
                    |_compositor, cx| {
                        if let Err(err) = copy_absolute_path(cx) {
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

struct FileLocationInfo {
    relative_path: String,
    absolute_path: String,
    relative_location: String,
    absolute_location: String,
    selection_summary: String,
}

impl FileLocationInfo {
    fn current(editor: &Editor) -> anyhow::Result<Self> {
        let (view, doc) = current_ref!(editor);
        let absolute_path = doc
            .path()
            .ok_or_else(|| anyhow!("Current buffer has no file path"))?;
        let relative_path = doc
            .relative_path()
            .unwrap_or(absolute_path)
            .display()
            .to_string();
        let absolute_path = absolute_path.display().to_string();

        let text = doc.text().slice(..);
        let primary = doc.selection(view.id).primary();
        let (start_line, end_line) = primary.line_range(text);
        let start_line = start_line + 1;
        let end_line = end_line + 1;

        let (relative_location, absolute_location, selection_summary) = if start_line == end_line {
            (
                format!("{relative_path}:{start_line}"),
                format!("{absolute_path}:{start_line}"),
                format!("line {start_line}"),
            )
        } else {
            (
                format!("{relative_path}:{start_line}-{end_line}"),
                format!("{absolute_path}:{start_line}-{end_line}"),
                format!("lines {start_line}-{end_line}"),
            )
        };

        Ok(Self {
            relative_path,
            absolute_path,
            relative_location,
            absolute_location,
            selection_summary,
        })
    }
}

fn render_file_location_markdown(info: &FileLocationInfo) -> String {
    let mut rendered = String::new();
    let _ = writeln!(rendered, "### File Location");
    let _ = writeln!(
        rendered,
        "`y` copy relative location, `a` absolute location, `r` relative path, `f` absolute path, `Esc` close\n"
    );
    let _ = writeln!(rendered, "**Selection**");
    let _ = writeln!(rendered, "`{}`\n", info.selection_summary);
    let _ = writeln!(rendered, "**Relative path**");
    let _ = writeln!(rendered, "`{}`\n", info.relative_path);
    let _ = writeln!(rendered, "**Absolute path**");
    let _ = writeln!(rendered, "`{}`\n", info.absolute_path);
    let _ = writeln!(rendered, "**Location**");
    let _ = writeln!(rendered, "`{}`", info.relative_location);
    rendered
}

fn write_clipboard(cx: &mut compositor::Context, text: String, status: &str) -> anyhow::Result<()> {
    cx.editor.registers.write('+', vec![text])?;
    cx.editor.set_status(status.to_string());
    Ok(())
}

fn copy_relative_location(cx: &mut compositor::Context) -> anyhow::Result<()> {
    let info = FileLocationInfo::current(cx.editor)?;
    write_clipboard(
        cx,
        info.relative_location,
        "Copied file location to system clipboard",
    )
}

fn copy_absolute_location(cx: &mut compositor::Context) -> anyhow::Result<()> {
    let info = FileLocationInfo::current(cx.editor)?;
    write_clipboard(
        cx,
        info.absolute_location,
        "Copied absolute file location to system clipboard",
    )
}

fn copy_relative_path(cx: &mut compositor::Context) -> anyhow::Result<()> {
    let info = FileLocationInfo::current(cx.editor)?;
    write_clipboard(
        cx,
        info.relative_path,
        "Copied relative file path to system clipboard",
    )
}

fn copy_absolute_path(cx: &mut compositor::Context) -> anyhow::Result<()> {
    let info = FileLocationInfo::current(cx.editor)?;
    write_clipboard(
        cx,
        info.absolute_path,
        "Copied absolute path to system clipboard",
    )
}

pub(crate) fn show_file_location(cx: &mut Context) {
    match FileLocationInfo::current(cx.editor) {
        Ok(info) => {
            let contents = FileLocationPopup::new(render_file_location_markdown(&info), cx.editor);
            let popup = Popup::new(FILE_LOCATION_POPUP_ID, contents).auto_close(true);
            cx.push_layer(Box::new(popup));
        }
        Err(err) => cx.editor.set_error(err.to_string()),
    }
}
