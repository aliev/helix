use super::*;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy)]
pub(crate) enum GitCommitMode {
    Create,
    Amend,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingGitCommit {
    pub repo_root: PathBuf,
    pub message_path: PathBuf,
    pub mode: GitCommitMode,
    pub saved_once: bool,
}

static PENDING_GIT_COMMITS: Lazy<Mutex<HashMap<DocumentId, PendingGitCommit>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub(crate) fn git_commit(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event == PromptEvent::Validate && is_pending_git_commit(view!(cx.editor).doc) {
        return submit_active_git_commit(cx);
    }
    open_git_commit_buffer(cx, GitCommitMode::Create, event)
}

pub(crate) fn git_commit_amend(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event == PromptEvent::Validate && is_pending_git_commit(view!(cx.editor).doc) {
        return submit_active_git_commit(cx);
    }
    open_git_commit_buffer(cx, GitCommitMode::Amend, event)
}

pub(crate) fn mark_commit_message_saved(doc_id: DocumentId) {
    if let Ok(mut pending) = PENDING_GIT_COMMITS.lock() {
        if let Some(session) = pending.get_mut(&doc_id) {
            session.saved_once = true;
        }
    }
}

pub(crate) fn take_pending_git_commit(doc_id: DocumentId) -> Option<PendingGitCommit> {
    PENDING_GIT_COMMITS.lock().ok()?.remove(&doc_id)
}

pub(crate) fn is_pending_git_commit(doc_id: DocumentId) -> bool {
    PENDING_GIT_COMMITS
        .lock()
        .map(|pending| pending.contains_key(&doc_id))
        .unwrap_or(false)
}

fn submit_active_git_commit(cx: &mut compositor::Context) -> anyhow::Result<()> {
    let doc_id = view!(cx.editor).doc;
    ensure!(
        is_pending_git_commit(doc_id),
        "No active git commit message buffer"
    );

    write_impl(
        cx,
        None,
        WriteOptions {
            force: false,
            auto_format: true,
        },
    )?;

    buffer_close_by_ids_impl(cx, &[doc_id], false)
}

fn open_git_commit_buffer(
    cx: &mut compositor::Context,
    mode: GitCommitMode,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let repo_root = git_repo_root(current_repo_dir(cx.editor))?;
    let git_dir = git_dir(&repo_root)?;
    let message_path = git_commit_message_path(&git_dir, mode);
    let template = commit_message_template(&repo_root, mode)?;
    std::fs::write(&message_path, template)?;

    let doc_id = cx.editor.open(&message_path, Action::HorizontalSplit)?;
    let doc = doc_mut!(cx.editor, &doc_id);
    doc.set_path(Some(&message_path));

    PENDING_GIT_COMMITS.lock().unwrap().insert(
        doc_id,
        PendingGitCommit {
            repo_root,
            message_path,
            mode,
            saved_once: false,
        },
    );

    cx.editor.set_status(match mode {
        GitCommitMode::Create => {
            "Opened git commit message. Use Space g m or :write-buffer-close to commit."
        }
        GitCommitMode::Amend => {
            "Opened amend message. Use Space g M or :write-buffer-close to amend the last commit."
        }
    });
    Ok(())
}

fn current_repo_dir(editor: &Editor) -> PathBuf {
    let (_view, doc) = current_ref!(editor);
    doc.path()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn git_repo_root(start_dir: PathBuf) -> anyhow::Result<PathBuf> {
    let output = Command::new("git")
        .current_dir(&start_dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .with_context(|| format!("failed to find git repository from {}", start_dir.display()))?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
}

fn git_dir(repo_root: &Path) -> anyhow::Result<PathBuf> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["rev-parse", "--git-dir"])
        .output()
        .with_context(|| format!("failed to locate .git directory for {}", repo_root.display()))?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let git_dir = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    Ok(if git_dir.is_absolute() {
        git_dir
    } else {
        repo_root.join(git_dir)
    })
}

fn git_commit_message_path(git_dir: &Path, mode: GitCommitMode) -> PathBuf {
    match mode {
        GitCommitMode::Create => git_dir.join("HELIX_COMMIT_EDITMSG"),
        GitCommitMode::Amend => git_dir.join("HELIX_COMMIT_EDITMSG_AMEND"),
    }
}

fn commit_message_template(repo_root: &Path, mode: GitCommitMode) -> anyhow::Result<String> {
    let mut out = String::new();
    if matches!(mode, GitCommitMode::Amend) {
        out.push_str(&last_commit_message(repo_root)?);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    } else if !has_staged_changes(repo_root)? {
        bail!("No staged changes to commit");
    }

    out.push_str(match mode {
        GitCommitMode::Create => {
            "# Use Space g m or :write-buffer-close to save this message and run git commit.\n"
        }
        GitCommitMode::Amend => {
            "# Use Space g M or :write-buffer-close to save this message and amend the last commit.\n"
        }
    });
    out.push_str("# Lines starting with # are ignored.\n");
    out.push_str("#\n");
    let staged = staged_changes_summary(repo_root)?;
    if staged.trim().is_empty() {
        out.push_str("# No staged file changes.\n");
    } else {
        out.push_str("# Staged changes:\n");
        for line in staged.lines() {
            let _ = writeln!(out, "#   {line}");
        }
    }
    Ok(out)
}

fn has_staged_changes(repo_root: &Path) -> anyhow::Result<bool> {
    let status = Command::new("git")
        .current_dir(repo_root)
        .args(["diff", "--cached", "--quiet"])
        .status()
        .with_context(|| format!("failed to inspect staged changes in {}", repo_root.display()))?;
    match status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => bail!("failed to inspect staged changes in {}", repo_root.display()),
    }
}

fn staged_changes_summary(repo_root: &Path) -> anyhow::Result<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["diff", "--cached", "--name-status"])
        .output()
        .with_context(|| format!("failed to read staged changes in {}", repo_root.display()))?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn last_commit_message(repo_root: &Path) -> anyhow::Result<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["log", "-1", "--format=%B"])
        .output()
        .with_context(|| format!("failed to read last commit message in {}", repo_root.display()))?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim_end().to_string())
}
