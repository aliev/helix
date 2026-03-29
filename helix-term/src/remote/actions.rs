use helix_core::{pos_at_coords, Position, Selection};
use helix_stdx::path::get_relative_path;
use helix_view::{align_view, Align, Editor};
use serde::Deserialize;
use serde_json::Value;
use std::{
    io::Write,
    path::PathBuf,
};

use crate::{
    commands::typed,
    remote::{
        CurrentDocumentSnapshot, DiagnosticSnapshot, GetCurrentDocumentArgs, GetDiagnosticsArgs,
        GetSelectionsArgs, GotoLocationArgs, IpcResponse, LayoutSnapshot, McpPresenceArgs,
        OpenDocumentSnapshot, OpenFileArgs, RemoteCommand, SelectLinesArgs, SelectionSnapshot,
        SplitDirection, SplitOpenArgs, FocusSplitArgs, ViewLayoutSnapshot,
    },
};

pub fn handle(editor: &mut Editor, command: RemoteCommand, arguments: Option<Value>) -> IpcResponse {
    match command {
        RemoteCommand::ReloadAll => match typed::reload_all_documents(editor) {
            Ok(reloaded) => {
                let message = format!("reloaded {reloaded} document(s)");
                editor.set_status(message.clone());
                IpcResponse::ok(message)
            }
            Err(err) => {
                let message = err.to_string();
                editor.set_error(message.clone());
                IpcResponse::err(message)
            }
        },
        RemoteCommand::GetActiveContext => match serde_json::to_value(active_context_snapshot(editor)) {
            Ok(data) => IpcResponse::ok_with_data("active editor context", data),
            Err(err) => IpcResponse::err(err.to_string()),
        },
        RemoteCommand::GetLayout => match serde_json::to_value(layout_snapshot(editor)) {
            Ok(data) => IpcResponse::ok_with_data("editor layout snapshot", data),
            Err(err) => IpcResponse::err(err.to_string()),
        },
        RemoteCommand::GetCurrentDocument => match parse_args::<GetCurrentDocumentArgs>(arguments) {
            Ok(args) => match current_document_snapshot(editor, args.path.as_deref()) {
                Ok(snapshot) => match serde_json::to_value(snapshot) {
                    Ok(data) => IpcResponse::ok_with_data("current document snapshot", data),
                    Err(err) => IpcResponse::err(err.to_string()),
                },
                Err(err) => IpcResponse::err(err.to_string()),
            },
            Err(err) => IpcResponse::err(err.to_string()),
        },
        RemoteCommand::GetOpenDocuments => match serde_json::to_value(open_documents_snapshot(editor)) {
            Ok(data) => IpcResponse::ok_with_data("open document snapshots", data),
            Err(err) => IpcResponse::err(err.to_string()),
        },
        RemoteCommand::GetSelections => match parse_args::<GetSelectionsArgs>(arguments) {
            Ok(args) => match selections_snapshot(editor, args.path.as_deref()) {
                Ok(snapshot) => match serde_json::to_value(snapshot) {
                    Ok(data) => IpcResponse::ok_with_data("selection snapshots", data),
                    Err(err) => IpcResponse::err(err.to_string()),
                },
                Err(err) => IpcResponse::err(err.to_string()),
            },
            Err(err) => IpcResponse::err(err.to_string()),
        },
        RemoteCommand::OpenFile => match parse_args::<OpenFileArgs>(arguments) {
            Ok(args) => match open_file(editor, args) {
                Ok(message) => IpcResponse::ok(message),
                Err(err) => IpcResponse::err(err.to_string()),
            },
            Err(err) => IpcResponse::err(err.to_string()),
        },
        RemoteCommand::SplitOpen => match parse_args::<SplitOpenArgs>(arguments) {
            Ok(args) => match split_open(editor, args) {
                Ok(message) => IpcResponse::ok(message),
                Err(err) => IpcResponse::err(err.to_string()),
            },
            Err(err) => IpcResponse::err(err.to_string()),
        },
        RemoteCommand::FocusSplit => match parse_args::<FocusSplitArgs>(arguments) {
            Ok(args) => match focus_split(editor, args.direction) {
                Ok(message) => IpcResponse::ok(message),
                Err(err) => IpcResponse::err(err.to_string()),
            },
            Err(err) => IpcResponse::err(err.to_string()),
        },
        RemoteCommand::CloseSplit => match close_split(editor) {
            Ok(message) => IpcResponse::ok(message),
            Err(err) => IpcResponse::err(err.to_string()),
        },
        RemoteCommand::GotoLocation => match parse_args::<GotoLocationArgs>(arguments) {
            Ok(args) => match goto_location(editor, args) {
                Ok(message) => IpcResponse::ok(message),
                Err(err) => IpcResponse::err(err.to_string()),
            },
            Err(err) => IpcResponse::err(err.to_string()),
        },
        RemoteCommand::SelectLines => match parse_args::<SelectLinesArgs>(arguments) {
            Ok(args) => match select_lines(editor, args) {
                Ok(message) => IpcResponse::ok(message),
                Err(err) => IpcResponse::err(err.to_string()),
            },
            Err(err) => IpcResponse::err(err.to_string()),
        },
        RemoteCommand::GetDiagnostics => match parse_args::<GetDiagnosticsArgs>(arguments) {
            Ok(args) => match diagnostics_snapshot(editor, args.path.as_deref()) {
                Ok(snapshot) => match serde_json::to_value(snapshot) {
                    Ok(data) => IpcResponse::ok_with_data("current diagnostics snapshot", data),
                    Err(err) => IpcResponse::err(err.to_string()),
                },
                Err(err) => IpcResponse::err(err.to_string()),
            },
            Err(err) => IpcResponse::err(err.to_string()),
        },
        RemoteCommand::UpdateMcpPresence => match parse_args::<McpPresenceArgs>(arguments) {
            Ok(args) => {
                editor.record_mcp_client_presence(args.client_id, args.client_name);
                IpcResponse::ok("updated MCP client presence")
            }
            Err(err) => IpcResponse::err(err.to_string()),
        },
    }
}

pub fn notify_attention(command: RemoteCommand, message: &str) {
    let should_notify = matches!(
        command,
        RemoteCommand::ReloadAll
            | RemoteCommand::OpenFile
            | RemoteCommand::SplitOpen
            | RemoteCommand::FocusSplit
            | RemoteCommand::CloseSplit
            | RemoteCommand::GotoLocation
            | RemoteCommand::SelectLines
    );
    if !should_notify {
        return;
    }

    let mut title = format!("Helix remote: {message}");
    title.retain(|ch| !matches!(ch, '\x07' | '\x1b' | '\n' | '\r'));
    if title.is_empty() {
        return;
    }

    let _ = std::io::stdout()
        .lock()
        .write_all(format!("\x1b]9;{title}\x1b\\").as_bytes());
    let _ = std::io::stdout().lock().flush();
}

pub fn current_document_snapshot(
    editor: &Editor,
    path: Option<&str>,
) -> anyhow::Result<CurrentDocumentSnapshot> {
    let (view, doc) = find_target_document(editor, path)?;
    let text = doc.text().slice(..);
    let primary_selection_text = doc
        .selection(view.id)
        .primary()
        .fragment(text)
        .to_string();

    Ok(CurrentDocumentSnapshot {
        path: doc.path().map(|path| path.display().to_string()),
        relative_path: doc
            .path()
            .map(|path| get_relative_path(path).display().to_string()),
        language: doc.language_name().map(ToOwned::to_owned),
        modified: doc.is_modified(),
        line_count: text.len_lines(),
        selections: SelectionSnapshot::from_selection(doc.selection(view.id), text),
        primary_selection_text,
        text: text.to_string(),
    })
}

pub fn active_context_snapshot(editor: &Editor) -> serde_json::Value {
    let (view, doc) = current_ref!(editor);
    let text = doc.text().slice(..);
    let selections = SelectionSnapshot::from_selection(doc.selection(view.id), text);
    let diagnostics: Vec<_> = doc
        .diagnostics()
        .iter()
        .map(|diagnostic| DiagnosticSnapshot {
            path: doc.path().map(|path| path.display().to_string()),
            line: text.char_to_line(diagnostic.range.start) + 1,
            end_line: text.char_to_line(diagnostic.range.end) + 1,
            message: diagnostic.message.clone(),
            severity: diagnostic
                .severity
                .map(|severity| format!("{severity:?}").to_lowercase()),
            source: diagnostic.source.clone(),
        })
        .collect();

    serde_json::json!({
        "document": current_document_snapshot(editor, None).expect("current document is always available"),
        "selections": selections,
        "diagnostics": diagnostics,
    })
}

pub fn layout_snapshot(editor: &Editor) -> LayoutSnapshot {
    let focused_view_id = format!("{:?}", editor.tree.focus);
    let views = editor
        .tree
        .views()
        .map(|(view, is_focused)| {
            let doc = editor.document(view.doc).expect("view document must exist");
            ViewLayoutSnapshot {
                view_id: format!("{:?}", view.id),
                is_focused,
                path: doc.path().map(|path| path.display().to_string()),
                relative_path: doc
                    .path()
                    .map(|path| get_relative_path(path).display().to_string()),
                x: view.area.x,
                y: view.area.y,
                width: view.area.width,
                height: view.area.height,
                position_hint: position_hint(view.area.x, view.area.y),
            }
        })
        .collect::<Vec<_>>();

    LayoutSnapshot {
        split_count: views.len(),
        focused_view_id,
        views,
    }
}

fn position_hint(x: u16, y: u16) -> String {
    match (x, y) {
        (0, 0) => "top-left".to_string(),
        (_, 0) => "top".to_string(),
        (0, _) => "left".to_string(),
        _ => "inner".to_string(),
    }
}

pub fn open_documents_snapshot(editor: &Editor) -> Vec<OpenDocumentSnapshot> {
    let current_id = view!(editor).doc;

    editor
        .documents()
        .map(|doc| OpenDocumentSnapshot {
            path: doc.path().map(|path| path.display().to_string()),
            relative_path: doc
                .path()
                .map(|path| get_relative_path(path).display().to_string()),
            modified: doc.is_modified(),
            is_current: doc.id() == current_id,
            line_count: doc.text().len_lines(),
        })
        .collect()
}

pub fn diagnostics_snapshot(
    editor: &Editor,
    path: Option<&str>,
) -> anyhow::Result<Vec<DiagnosticSnapshot>> {
    let (_view, doc) = find_target_document(editor, path)?;
    let text = doc.text().slice(..);

    Ok(doc
        .diagnostics()
        .iter()
        .map(|diagnostic| DiagnosticSnapshot {
            path: doc.path().map(|path| path.display().to_string()),
            line: text.char_to_line(diagnostic.range.start) + 1,
            end_line: text.char_to_line(diagnostic.range.end) + 1,
            message: diagnostic.message.clone(),
            severity: diagnostic
                .severity
                .map(|severity| format!("{severity:?}").to_lowercase()),
            source: diagnostic.source.clone(),
        })
        .collect())
}

pub fn selections_snapshot(
    editor: &Editor,
    path: Option<&str>,
) -> anyhow::Result<Vec<SelectionSnapshot>> {
    let (view, doc) = find_target_document(editor, path)?;
    Ok(SelectionSnapshot::from_selection(
        doc.selection(view.id),
        doc.text().slice(..),
    ))
}

pub fn parse_args<T>(arguments: Option<Value>) -> anyhow::Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(arguments.unwrap_or_else(|| Value::Object(Default::default())))
        .map_err(Into::into)
}

pub fn open_file(editor: &mut Editor, args: OpenFileArgs) -> anyhow::Result<String> {
    let path = resolve_path(&args.path);
    editor
        .open(&path, helix_view::editor::Action::Replace)
        .map_err(|err| anyhow::anyhow!("failed to open file {}: {err}", path.display()))?;

    if args.line.is_some() || args.column.is_some() {
        goto_current_position(editor, args.line.unwrap_or(1), args.column.unwrap_or(1))?;
    }

    Ok(format!("opened {}", path.display()))
}

pub fn goto_location(editor: &mut Editor, args: GotoLocationArgs) -> anyhow::Result<String> {
    if let Some(path) = args.path {
        open_file(
            editor,
            OpenFileArgs {
                path,
                line: None,
                column: None,
            },
        )?;
    }

    goto_current_position(editor, args.line, args.column.unwrap_or(1))?;
    Ok(format!(
        "moved cursor to {}:{}",
        args.line,
        args.column.unwrap_or(1)
    ))
}

pub fn split_open(editor: &mut Editor, args: SplitOpenArgs) -> anyhow::Result<String> {
    let path = resolve_path(&args.path);
    let action = match args.direction {
        SplitDirection::Left | SplitDirection::Right => helix_view::editor::Action::VerticalSplit,
        SplitDirection::Up | SplitDirection::Down => helix_view::editor::Action::HorizontalSplit,
    };

    editor
        .open(&path, action)
        .map_err(|err| anyhow::anyhow!("failed to open file {}: {err}", path.display()))?;

    if matches!(args.direction, SplitDirection::Left | SplitDirection::Up) {
        editor.swap_split_in_direction(args.direction.focus_direction());
    }

    if args.line.is_some() || args.column.is_some() {
        goto_current_position(editor, args.line.unwrap_or(1), args.column.unwrap_or(1))?;
    }

    Ok(format!(
        "opened {} in {} split",
        path.display(),
        direction_name(args.direction)
    ))
}

pub fn focus_split(editor: &mut Editor, direction: SplitDirection) -> anyhow::Result<String> {
    let before = editor.tree.focus;
    editor.focus_direction(direction.focus_direction());
    anyhow::ensure!(
        editor.tree.focus != before,
        "no split exists in the {} direction",
        direction_name(direction)
    );
    Ok(format!("focused {} split", direction_name(direction)))
}

pub fn close_split(editor: &mut Editor) -> anyhow::Result<String> {
    anyhow::ensure!(editor.tree.views().count() > 1, "cannot close the only split");
    let view_id = editor.tree.focus;
    editor.close(view_id);
    Ok("closed current split".to_string())
}

pub fn select_lines(editor: &mut Editor, args: SelectLinesArgs) -> anyhow::Result<String> {
    let start_line = args
        .resolved_start_line()
        .ok_or_else(|| anyhow::anyhow!("missing field `start_line` or `line`"))?;
    let end_line = args.end_line.unwrap_or(start_line);

    if let Some(path) = args.path {
        open_file(
            editor,
            OpenFileArgs {
                path,
                line: None,
                column: None,
            },
        )?;
    }

    anyhow::ensure!(start_line > 0, "start_line must be greater than 0");
    anyhow::ensure!(end_line > 0, "end_line must be greater than 0");
    anyhow::ensure!(
        end_line >= start_line,
        "end_line must be greater than or equal to start_line"
    );

    let (view, doc) = current!(editor);
    let text = doc.text();
    let len_lines = text.len_lines();
    anyhow::ensure!(start_line <= len_lines, "start_line is past the end of the document");

    let start = text.line_to_char(start_line - 1);
    let end = text.line_to_char(end_line.min(len_lines));
    doc.set_selection(view.id, Selection::single(start, end));
    align_view(doc, view, Align::Center);

    if start_line == end_line {
        Ok(format!("selected line {start_line}"))
    } else {
        Ok(format!("selected lines {start_line}-{end_line}"))
    }
}

fn goto_current_position(editor: &mut Editor, line: usize, column: usize) -> anyhow::Result<()> {
    anyhow::ensure!(line > 0, "line must be greater than 0");
    anyhow::ensure!(column > 0, "column must be greater than 0");

    let (view, doc) = current!(editor);
    let coords = Position::new(line - 1, column - 1);
    let pos = pos_at_coords(doc.text().slice(..), coords, true);
    doc.set_selection(view.id, Selection::point(pos));
    align_view(doc, view, Align::Center);
    Ok(())
}

fn resolve_path(path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        helix_stdx::env::current_working_dir().join(path)
    }
}

fn direction_name(direction: SplitDirection) -> &'static str {
    match direction {
        SplitDirection::Left => "left",
        SplitDirection::Right => "right",
        SplitDirection::Up => "up",
        SplitDirection::Down => "down",
    }
}

fn find_target_document<'a>(
    editor: &'a Editor,
    path: Option<&str>,
) -> anyhow::Result<(&'a helix_view::View, &'a helix_view::Document)> {
    match path.map(resolve_path) {
        Some(resolved) => {
            let doc = editor
                .documents()
                .find(|doc| doc.path().is_some_and(|doc_path| *doc_path == resolved))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "file is not open in the current Helix session: {}",
                        resolved.display()
                    )
                })?;

            let view = editor
                .tree
                .views()
                .find(|(view, _)| view.doc == doc.id())
                .map(|(view, _)| view)
                .ok_or_else(|| anyhow::anyhow!("file is open but not visible in any view"))?;

            Ok((view, doc))
        }
        None => Ok(current_ref!(editor)),
    }
}
