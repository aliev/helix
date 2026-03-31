use anyhow::Context;
use helix_event::register_hook;
use helix_view::events::DocumentDidClose;

use crate::commands::git_commit::{take_pending_git_commit, GitCommitMode};
use crate::commands::typed;
use crate::handlers::Handlers;
use crate::job::{self, Callback};

fn strip_git_comments(message: &str) -> String {
    message
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn handle_pending_git_commit_close(event: &mut DocumentDidClose<'_>) -> anyhow::Result<()> {
    let Some(session) = take_pending_git_commit(event.doc.id()) else {
        return Ok(());
    };

    if event.doc.is_modified() {
        let _ = std::fs::remove_file(&session.message_path);
        event
            .editor
            .set_status("Git commit cancelled: message buffer closed with unsaved changes");
        return Ok(());
    }

    if !session.saved_once {
        let _ = std::fs::remove_file(&session.message_path);
        event
            .editor
            .set_status("Git commit cancelled: save the message before closing");
        return Ok(());
    }

    let message = std::fs::read_to_string(&session.message_path)?;
    if strip_git_comments(&message).is_empty() {
        let _ = std::fs::remove_file(&session.message_path);
        event
            .editor
            .set_status("Git commit cancelled: commit message is empty");
        return Ok(());
    }

    let repo_root = session.repo_root.clone();
    let message_path = session.message_path.clone();
    let mode = session.mode;
    tokio::spawn(async move {
        let result = run_git_commit(repo_root, message_path.clone(), mode).await;
        let callback = Callback::Editor(Box::new(move |editor| match result {
            Ok(status) => {
                let _ = std::fs::remove_file(&message_path);
                match typed::reload_all_documents(editor) {
                    Ok(reloaded) => {
                        editor.set_status(format!("{status} • reloaded {reloaded} document(s)"))
                    }
                    Err(err) => editor.set_error(format!(
                        "{status}, but reload-all failed: {err}"
                    )),
                }
            }
            Err(err) => editor.set_error(format!(
                "{} (commit message kept at {})",
                err,
                message_path.display()
            )),
        }));
        job::dispatch_callback(callback).await;
    });

    Ok(())
}

async fn run_git_commit(
    repo_root: std::path::PathBuf,
    message_path: std::path::PathBuf,
    mode: GitCommitMode,
) -> anyhow::Result<String> {
    use tokio::process::Command;

    let mut command = Command::new("git");
    command.current_dir(&repo_root);
    command.args(["commit", "--cleanup=strip", "-F"]);
    command.arg(&message_path);
    if matches!(mode, GitCommitMode::Amend) {
        command.arg("--amend");
    }
    let output = command.output().await.with_context(|| {
        format!(
            "failed to run git commit in {}",
            repo_root.as_path().display()
        )
    })?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout.lines().next().unwrap_or("Commit created");
    Ok(first_line.trim().to_string())
}

pub(super) fn register_hooks(_handlers: &Handlers) {
    register_hook!(move |event: &mut DocumentDidClose<'_>| { handle_pending_git_commit_close(event) });
}
