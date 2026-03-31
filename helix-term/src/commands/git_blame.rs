use super::*;
use chrono::{Local, TimeZone};
use helix_view::graphics::Rect;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) const GIT_LINE_BLAME_PREVIEW_ID: &str = "git-line-blame-preview";

#[derive(Clone)]
struct GitLineBlameInfo {
    repo_root: PathBuf,
    relative_path: String,
    remote_base_url: String,
    line_number: usize,
    commit: String,
    author: String,
    summary: String,
    description: String,
    date: String,
}

impl GitLineBlameInfo {
    fn is_uncommitted(&self) -> bool {
        self.commit.chars().all(|ch| ch == '0')
    }

    fn short_commit(&self) -> &str {
        if self.commit.len() > 8 {
            &self.commit[..8]
        } else {
            &self.commit
        }
    }

    fn commit_url(&self) -> anyhow::Result<String> {
        anyhow::ensure!(
            !self.is_uncommitted(),
            "Current line is not committed yet, so there is no remote commit URL"
        );
        Ok(format!("{}/commit/{}", self.remote_base_url, self.commit))
    }

    fn line_permalink(&self) -> anyhow::Result<String> {
        anyhow::ensure!(
            !self.is_uncommitted(),
            "Current line is not committed yet, so there is no blamed-line permalink"
        );
        Ok(format!(
            "{}/blob/{}/{}#L{}",
            self.remote_base_url, self.commit, self.relative_path, self.line_number
        ))
    }

    fn diff_command_summary(&self) -> String {
        if self.is_uncommitted() {
            format!("git diff -- {}", self.relative_path)
        } else {
            format!("git show {} -- {}", self.short_commit(), self.relative_path)
        }
    }
}

pub(crate) struct GitLineBlamePopup {
    markdown: ui::Markdown,
}

impl GitLineBlamePopup {
    fn new(contents: String, editor: &Editor) -> Self {
        Self {
            markdown: ui::Markdown::new(contents, editor.syn_loader.clone()),
        }
    }
}

impl Component for GitLineBlamePopup {
    fn handle_event(
        &mut self,
        event: &compositor::Event,
        cx: &mut compositor::Context,
    ) -> compositor::EventResult {
        match event {
            compositor::Event::Key(key) if key.modifiers.is_empty() => match key.code {
                KeyCode::Char('g') => compositor::EventResult::Consumed(Some(Box::new(
                    |_compositor, cx| {
                        if let Err(err) =
                            copy_git_line_blame_remote(cx, Args::default(), PromptEvent::Validate)
                        {
                            cx.editor.set_error(err.to_string());
                        }
                    },
                ))),
                KeyCode::Char('l') => compositor::EventResult::Consumed(Some(Box::new(
                    |_compositor, cx| {
                        if let Err(err) = copy_git_line_blame_permalink(
                            cx,
                            Args::default(),
                            PromptEvent::Validate,
                        ) {
                            cx.editor.set_error(err.to_string());
                        }
                    },
                ))),
                KeyCode::Char('d') => compositor::EventResult::Consumed(Some(Box::new(
                    |_compositor, cx| {
                        if let Err(err) =
                            show_git_line_blame_diff(cx, Args::default(), PromptEvent::Validate)
                        {
                            cx.editor.set_error(err.to_string());
                        }
                    },
                ))),
                KeyCode::Char('y') => compositor::EventResult::Consumed(Some(Box::new(
                    |_compositor, cx| {
                        if let Err(err) =
                            copy_git_line_blame_sha(cx, Args::default(), PromptEvent::Validate)
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

fn current_git_blame_info(editor: &Editor) -> anyhow::Result<GitLineBlameInfo> {
    let (view, doc) = current_ref!(editor);
    let path = doc
        .path()
        .ok_or_else(|| anyhow!("Current buffer has no file path"))?;
    let permalink = editor
        .diff_providers
        .get_permalink_info(path)
        .ok_or_else(|| anyhow!("Git blame is not available for the current file"))?;
    let relative_path = path
        .strip_prefix(&permalink.repo_root)
        .context("Current file is not inside the repository root")?
        .to_string_lossy()
        .replace('\\', "/");
    let line_number = doc.selection(view.id).primary().cursor_line(doc.text().slice(..)) + 1;
    let output = Command::new("git")
        .current_dir(&permalink.repo_root)
        .args([
            "blame",
            "--line-porcelain",
            "-L",
            &format!("{line_number},{line_number}"),
            "--",
            &relative_path,
        ])
        .output()
        .with_context(|| format!("failed to run git blame for {}", path.display()))?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    );
    let commit_description = if output.stdout.starts_with(b"00000000") {
        String::new()
    } else {
        let show_output = Command::new("git")
            .current_dir(&permalink.repo_root)
            .args(["show", "-s", "--format=%B", output_commit_hash(&output.stdout)?, "--"])
            .output()
            .with_context(|| format!("failed to read commit message for {}", path.display()))?;
        if show_output.status.success() {
            String::from_utf8_lossy(&show_output.stdout).trim().to_string()
        } else {
            String::new()
        }
    };
    parse_git_blame_output(
        &permalink.repo_root,
        &remote_to_web_url_for_blame(&permalink.remote_url)?,
        &relative_path,
        line_number,
        &String::from_utf8_lossy(&output.stdout),
        &commit_description,
    )
}

fn remote_to_web_url_for_blame(remote: &str) -> anyhow::Result<String> {
    let remote = remote.trim();

    if let Some((host, path)) = remote
        .split_once("://")
        .and_then(|_| {
            let url = Url::parse(remote).ok()?;
            let host = url.host_str()?.to_owned();
            Some((host, url.path().trim_start_matches('/').to_owned()))
        })
        .or_else(|| {
            let (user_host, path) = remote.split_once(':')?;
            let host = user_host.split('@').next_back()?.to_owned();
            Some((host, path.to_owned()))
        })
    {
        let path = path.trim_end_matches(".git").trim_matches('/');
        return Ok(format!("https://{host}/{path}"));
    }

    bail!("Unsupported git remote URL: {remote}");
}

fn output_commit_hash(output: &[u8]) -> anyhow::Result<&str> {
    let line = std::str::from_utf8(output)
        .context("git blame returned non-utf8 output")?
        .lines()
        .next()
        .ok_or_else(|| anyhow!("git blame returned empty output"))?;
    line.split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("git blame did not return a commit hash"))
}

fn parse_git_blame_output(
    repo_root: &Path,
    remote_base_url: &str,
    relative_path: &str,
    line_number: usize,
    output: &str,
    commit_description: &str,
) -> anyhow::Result<GitLineBlameInfo> {
    let mut commit = None;
    let mut author = None;
    let mut summary = None;
    let mut author_time = None;

    for (idx, line) in output.lines().enumerate() {
        if idx == 0 {
            commit = line.split_whitespace().next().map(str::to_owned);
            continue;
        }
        if let Some(rest) = line.strip_prefix("author ") {
            author = Some(rest.to_owned());
        } else if let Some(rest) = line.strip_prefix("author-time ") {
            author_time = rest.parse::<i64>().ok();
        } else if let Some(rest) = line.strip_prefix("summary ") {
            summary = Some(rest.to_owned());
        }
    }

    let commit = commit.ok_or_else(|| anyhow!("git blame did not return a commit hash"))?;
    let date = author_time
        .and_then(|timestamp| Local.timestamp_opt(timestamp, 0).single())
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown date".to_string());

    Ok(GitLineBlameInfo {
        repo_root: repo_root.to_path_buf(),
        relative_path: relative_path.to_string(),
        remote_base_url: remote_base_url.to_string(),
        line_number,
        commit,
        author: author.unwrap_or_else(|| "Unknown author".to_string()),
        summary: summary.unwrap_or_else(|| "No commit summary".to_string()),
        description: commit_description.to_string(),
        date,
    })
}

fn render_git_line_blame_markdown(blame: &GitLineBlameInfo) -> String {
    let mut rendered = String::new();
    let _ = writeln!(rendered, "### Blame  L{}", blame.line_number);
    let _ = writeln!(
        rendered,
        "`g` remote, `l` permalink, `d` diff, `y` copy sha, `Esc` close\n"
    );
    let _ = writeln!(
        rendered,
        "`{}`  {}  {}",
        blame.short_commit(),
        blame.author,
        blame.date
    );
    let _ = writeln!(rendered, "**{}**\n", blame.summary);
    let description = blame.description.trim();
    if !description.is_empty() {
        let body = description
            .strip_prefix(&blame.summary)
            .map(str::trim_start)
            .unwrap_or(description);
        if !body.is_empty() {
            let _ = writeln!(rendered, "{}\n", body);
        }
    }
    rendered
}

pub(crate) fn preview_git_line_blame(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let blame = current_git_blame_info(cx.editor)?;
    let callback = async move {
        let call: job::Callback = Callback::EditorCompositor(Box::new(
            move |editor: &mut Editor, compositor: &mut Compositor| {
                let contents =
                    GitLineBlamePopup::new(render_git_line_blame_markdown(&blame), editor);
                let popup = Popup::new(GIT_LINE_BLAME_PREVIEW_ID, contents)
                    .position(editor.cursor().0)
                    .position_bias(Open::Above)
                    .auto_close(false);
                compositor.replace_or_push(GIT_LINE_BLAME_PREVIEW_ID, popup);
            },
        ));
        Ok(call)
    };
    cx.jobs.callback(callback);
    cx.editor
        .set_status("Opened git line blame popup. Press Esc to close.");
    Ok(())
}

pub(crate) fn refresh_git_line_blame_preview(editor: &mut Editor, compositor: &mut Compositor) {
    match current_git_blame_info(editor) {
        Ok(blame) => {
            let contents = GitLineBlamePopup::new(render_git_line_blame_markdown(&blame), editor);
            let popup = Popup::new(GIT_LINE_BLAME_PREVIEW_ID, contents)
                .position(editor.cursor().0)
                .position_bias(Open::Above)
                .auto_close(false);
            compositor.replace_or_push(GIT_LINE_BLAME_PREVIEW_ID, popup);
        }
        Err(_) => {
            compositor.remove(GIT_LINE_BLAME_PREVIEW_ID);
        }
    }
}

fn copy_git_line_blame_remote(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let blame = current_git_blame_info(cx.editor)?;
    let url = blame.commit_url()?;
    cx.editor.registers.write('+', vec![url])?;
    cx.editor
        .set_status("Copied blamed commit URL to system clipboard");
    Ok(())
}

fn copy_git_line_blame_permalink(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let blame = current_git_blame_info(cx.editor)?;
    let url = blame.line_permalink()?;
    cx.editor.registers.write('+', vec![url])?;
    cx.editor
        .set_status("Copied blamed line permalink to system clipboard");
    Ok(())
}

fn copy_git_line_blame_sha(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let blame = current_git_blame_info(cx.editor)?;
    cx.editor.registers.write('+', vec![blame.commit.clone()])?;
    cx.editor
        .set_status("Copied blamed commit SHA to system clipboard");
    Ok(())
}

fn show_git_line_blame_diff(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let blame = current_git_blame_info(cx.editor)?;
    let shell = cx.editor.config().shell.clone();
    let repo_root = shell_single_quote_escape(blame.repo_root.display().to_string());
    let relative_path = shell_single_quote_escape(blame.relative_path.clone());
    let command = if blame.is_uncommitted() {
        format!("cd '{repo_root}' && git diff -- '{relative_path}'")
    } else {
        format!(
            "cd '{repo_root}' && git show {} -- '{relative_path}'",
            blame.commit,
        )
    };
    let summary = blame.diff_command_summary();

    let callback = async move {
        let output = shell_impl_async(&shell, &command, None).await?;
        let call: job::Callback = Callback::EditorCompositor(Box::new(
            move |editor: &mut Editor, compositor: &mut Compositor| {
                if !output.trim().is_empty() {
                    let contents = ui::Markdown::new(
                        format!("### {}\n\n```diff\n{}\n```", summary, output.trim_end()),
                        editor.syn_loader.clone(),
                    );
                    let popup = Popup::new("git-line-blame-diff", contents)
                        .position(editor.cursor().0)
                        .position_bias(Open::Above);
                    compositor.replace_or_push("git-line-blame-diff", popup);
                }
                editor.set_status("Opened blamed diff preview");
            },
        ));
        Ok(call)
    };
    cx.jobs.callback(callback);
    Ok(())
}

fn shell_single_quote_escape(value: String) -> String {
    value.replace('\'', r#"'\''"#)
}
