use std::fs;
use std::path::Path;

use helix_term::application::Application;
use helix_view::{doc, persistent_undo::PersistentUndoConfig};

use super::*;

/// A test config with persistent undo enabled, pointed at a temporary
/// directory so the tests never touch the real data directory.
fn persistent_undo_config(undo_dir: &Path) -> helix_term::config::Config {
    let mut config = helpers::test_config();
    config.editor.persistent_undo = PersistentUndoConfig {
        enable: true,
        dir: Some(undo_dir.to_path_buf()),
        ..PersistentUndoConfig::default()
    };
    config
}

#[tokio::test(flavor = "multi_thread")]
async fn test_undo_history_survives_a_restart() -> anyhow::Result<()> {
    let undo_dir = tempfile::tempdir()?;
    let file = helpers::temp_file_with_contents("hello\n")?;

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(&mut app, Some("A world<esc>:w<ret>"), None, false).await?;

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(file.path(), None)
        .build()?;
    {
        // A restored document must not look modified: `set_last_saved_revision`
        // in `restore` points the document's last-saved revision at the
        // restored `current`, which is not the root. Without it every
        // reopened file would show `[+]` and `:q` would refuse, even though
        // pressing `u` below (the only thing every other test in this file
        // checks) would still work.
        let doc = doc!(app.editor);
        assert!(
            !doc.is_modified(),
            "a document with history restored from disk must not be marked modified"
        );
    }
    test_key_sequence(
        &mut app,
        Some("u"),
        Some(&|app| {
            let doc = doc!(app.editor);
            assert_eq!(doc.text().to_string(), "hello\n");
        }),
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_undo_history_is_persisted_when_exiting() -> anyhow::Result<()> {
    // `:x` drains the save queue through `Editor::flush_writes`, which never
    // reaches `Application::handle_document_write`.
    let undo_dir = tempfile::tempdir()?;
    let file = helpers::temp_file_with_contents("hello\n")?;

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(&mut app, Some("A world<esc>:x<ret>"), None, true).await?;

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(
        &mut app,
        Some("u"),
        Some(&|app| {
            let doc = doc!(app.editor);
            assert_eq!(doc.text().to_string(), "hello\n");
        }),
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_editing_continues_into_restored_history() -> anyhow::Result<()> {
    let undo_dir = tempfile::tempdir()?;
    let file = helpers::temp_file_with_contents("hello\n")?;

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(&mut app, Some("A world<esc>:w<ret>"), None, false).await?;

    // A new edit, then two undos: the first walks back over this session's
    // edit, the second has to cross into the restored history.
    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(
        &mut app,
        Some("A!<esc>uu"),
        Some(&|app| {
            let doc = doc!(app.editor);
            assert_eq!(doc.text().to_string(), "hello\n");
        }),
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_undo_history_is_discarded_when_the_file_changed() -> anyhow::Result<()> {
    let undo_dir = tempfile::tempdir()?;
    let file = helpers::temp_file_with_contents("hello\n")?;

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(&mut app, Some("A world<esc>:w<ret>"), None, false).await?;

    fs::write(file.path(), "something else\n")?;

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(file.path(), None)
        .build()?;
    test_key_sequences(
        &mut app,
        vec![
            // A stray `u` with no history of its own must be a no-op, not the
            // discarded history's "hello world\n" -> "hello\n" undo.
            (
                Some("u"),
                Some(&|app: &Application| {
                    let doc = doc!(app.editor);
                    assert_eq!(doc.text().to_string(), "something else\n");
                }),
            ),
            // A real edit followed by two undos discriminates wiring being
            // present but correctly discarding the mismatched history from
            // wiring being entirely absent: if the stale history had been
            // restored, the second undo would walk into it and change the
            // text; here it must be a no-op instead, since this session's own
            // history has nothing more to undo.
            (
                Some("A!<esc>uu"),
                Some(&|app: &Application| {
                    let doc = doc!(app.editor);
                    assert_eq!(doc.text().to_string(), "something else\n");
                }),
            ),
        ],
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_undo_history_follows_a_save_as() -> anyhow::Result<()> {
    let undo_dir = tempfile::tempdir()?;
    let file = helpers::temp_file_with_contents("hello\n")?;
    // A path that does not exist yet, in its own directory: `NamedTempFile`
    // would create the file up front, and its mtime (later than `file`'s)
    // would trip the unrelated "file modified by an external process" guard
    // in `Document::save_impl` the first time this test saves to it.
    let other_dir = tempfile::tempdir()?;
    let other = other_dir.path().join("other");

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(file.path(), None)
        .build()?;
    let write_as = format!("A world<esc>:w {}<ret>", other.to_string_lossy());
    test_key_sequence(&mut app, Some(&write_as), None, false).await?;

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(&other, None)
        .build()?;
    test_key_sequence(
        &mut app,
        Some("u"),
        Some(&|app| {
            let doc = doc!(app.editor);
            assert_eq!(doc.text().to_string(), "hello\n");
        }),
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_undo_history_follows_a_save_as_and_quit() -> anyhow::Result<()> {
    // `:wq other.txt` (like `:x other.txt`, `:wq!` and `:x!`) drains the save
    // queue through `Editor::flush_writes`, which never calls `set_doc_path`:
    // the document's own path is still the old one when `DocumentDidSave`
    // fires, so `persist` must key off the event's `path` instead.
    let undo_dir = tempfile::tempdir()?;
    let file = helpers::temp_file_with_contents("hello\n")?;
    let other_dir = tempfile::tempdir()?;
    let other = other_dir.path().join("other");

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(file.path(), None)
        .build()?;
    let write_quit_as = format!("A world<esc>:wq {}<ret>", other.to_string_lossy());
    test_key_sequence(&mut app, Some(&write_quit_as), None, true).await?;

    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(undo_dir.path()))
        .with_file(&other, None)
        .build()?;
    test_key_sequence(
        &mut app,
        Some("u"),
        Some(&|app| {
            let doc = doc!(app.editor);
            assert_eq!(doc.text().to_string(), "hello\n");
        }),
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_tiny_budget_trims_history_but_keeps_undo_working() -> anyhow::Result<()> {
    let undo_dir = tempfile::tempdir()?;
    let file = helpers::temp_file_with_contents("hello\n")?;

    let mut config = persistent_undo_config(undo_dir.path());
    // One kibibyte: several of the edits below will not fit.
    config.editor.persistent_undo.max_memory_kib = 1;

    let mut app = helpers::AppBuilder::new()
        .with_config(config.clone())
        .with_file(file.path(), None)
        .build()?;
    // Four separate revisions, each a large enough insert to matter against a
    // 1 KiB budget. Walking up from the newest, the subtree each revision's
    // undo would have to restore costs 1600, 1200, 800 and then 400 bytes
    // (each `Delete` inversion carries no text and costs nothing) — a 1 KiB
    // budget keeps only the last two, so undo must run out before reaching
    // the original "hello\n".
    let big = "z".repeat(400);
    let keys = format!("A{big}<esc>A{big}<esc>A{big}<esc>A{big}<esc>:w<ret>");
    test_key_sequence(&mut app, Some(&keys), None, false).await?;

    let mut app = helpers::AppBuilder::new()
        .with_config(config)
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(
        &mut app,
        Some("uuuuuuuu"),
        Some(&|app: &Application| {
            let doc = doc!(app.editor);
            // Undo walks back as far as the trimmed history allows and then
            // stops. It must not panic, and it must not leave the buffer in a
            // state the document was never in: every reachable state is a
            // prefix-count of the appended blocks.
            let text = doc.text().to_string();
            assert!(
                text.starts_with("hello"),
                "unexpected buffer contents after undo: {text:?}"
            );
            let appended = text.len() - "hello\n".len();
            assert_eq!(appended % 400, 0, "buffer is not at a revision boundary");
            // The discriminating assertion: with the budget ignored, undo
            // would walk all the way back to the original "hello\n", which
            // also sits on a revision boundary (appended == 0) and would
            // satisfy the assertion above alone. The revisions that would
            // take the buffer back that far were trimmed away, so undo must
            // run out first and the buffer must not be the original.
            assert_ne!(
                text, "hello\n",
                "undo returned to the original contents; the budget did not trim any history"
            );
        }),
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_saving_succeeds_when_the_undo_directory_is_unusable() -> anyhow::Result<()> {
    // A plain file where the undo directory should be. Persisting the history
    // fails; saving the document must not.
    let undo_dir = tempfile::tempdir()?;
    let blocked = undo_dir.path().join("undo");
    fs::write(&blocked, "not a directory")?;

    let file = helpers::temp_file_with_contents("hello\n")?;
    let mut app = helpers::AppBuilder::new()
        .with_config(persistent_undo_config(&blocked))
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(&mut app, Some("A world<esc>:w<ret>"), None, false).await?;

    assert_eq!(fs::read_to_string(file.path())?, "hello world\n");

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_disabled_persistent_undo_writes_no_undo_file() -> anyhow::Result<()> {
    // `enable = false` is the default and where every user starts. Even with
    // `dir` pointed at a real, writable directory, saving must never create
    // anything in it.
    let undo_dir = tempfile::tempdir()?;
    let file = helpers::temp_file_with_contents("hello\n")?;

    let mut config = helpers::test_config();
    config.editor.persistent_undo = PersistentUndoConfig {
        enable: false,
        dir: Some(undo_dir.path().to_path_buf()),
        ..PersistentUndoConfig::default()
    };

    let mut app = helpers::AppBuilder::new()
        .with_config(config)
        .with_file(file.path(), None)
        .build()?;
    test_key_sequence(&mut app, Some("A world<esc>:w<ret>"), None, false).await?;

    // `create_dir_all` in `write_atomically` runs only on an actual write, so
    // with persistence disabled the directory is never even created.
    let entries: Vec<_> = match fs::read_dir(undo_dir.path()) {
        Ok(read_dir) => read_dir.collect::<Result<Vec<_>, _>>()?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err.into()),
    };
    assert!(
        entries.is_empty(),
        "persistent undo must not write anything when disabled, found: {entries:?}"
    );

    Ok(())
}
