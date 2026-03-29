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
        CurrentDocumentSnapshot, DiagnosticSnapshot, GotoLocationArgs, IpcResponse,
        OpenDocumentSnapshot, OpenFileArgs, RemoteCommand, SelectLinesArgs, SelectionSnapshot,
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
        RemoteCommand::GetCurrentDocument => match serde_json::to_value(current_document_snapshot(editor))
        {
            Ok(data) => IpcResponse::ok_with_data("current document snapshot", data),
            Err(err) => IpcResponse::err(err.to_string()),
        },
        RemoteCommand::GetOpenDocuments => match serde_json::to_value(open_documents_snapshot(editor)) {
            Ok(data) => IpcResponse::ok_with_data("open document snapshots", data),
            Err(err) => IpcResponse::err(err.to_string()),
        },
        RemoteCommand::GetSelections => {
            let (view, doc) = current!(editor);
            let selections =
                SelectionSnapshot::from_selection(doc.selection(view.id), doc.text().slice(..));
            match serde_json::to_value(selections) {
                Ok(data) => IpcResponse::ok_with_data("selection snapshots", data),
                Err(err) => IpcResponse::err(err.to_string()),
            }
        }
        RemoteCommand::OpenFile => match parse_args::<OpenFileArgs>(arguments) {
            Ok(args) => match open_file(editor, args) {
                Ok(message) => IpcResponse::ok(message),
                Err(err) => IpcResponse::err(err.to_string()),
            },
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
        RemoteCommand::GetDiagnostics => match serde_json::to_value(current_diagnostics_snapshot(editor))
        {
            Ok(data) => IpcResponse::ok_with_data("current diagnostics snapshot", data),
            Err(err) => IpcResponse::err(err.to_string()),
        },
    }
}

pub fn notify_attention(command: RemoteCommand, message: &str) {
    let should_notify = matches!(
        command,
        RemoteCommand::ReloadAll
            | RemoteCommand::OpenFile
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

pub fn current_document_snapshot(editor: &Editor) -> CurrentDocumentSnapshot {
    let (view, doc) = current_ref!(editor);
    let text = doc.text().slice(..);
    let primary_selection_text = doc
        .selection(view.id)
        .primary()
        .fragment(text)
        .to_string();

    CurrentDocumentSnapshot {
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

pub fn current_diagnostics_snapshot(editor: &Editor) -> Vec<DiagnosticSnapshot> {
    let (_view, doc) = current_ref!(editor);
    let text = doc.text().slice(..);

    doc.diagnostics()
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
        .collect()
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

pub fn select_lines(editor: &mut Editor, args: SelectLinesArgs) -> anyhow::Result<String> {
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

    let start_line = args.start_line;
    let end_line = args.end_line.unwrap_or(start_line);
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
