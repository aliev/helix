use std::fs;
use std::path::Path;

use helix_view::{doc, persistent_undo::PersistentUndoConfig};

use super::*;

/// A test config with persistent undo enabled, pointed at a temporary
/// directory so the tests never touch the real data directory.
fn persistent_undo_config(undo_dir: &Path) -> helix_term::config::Config {
    let mut config = helpers::test_config();
    config.editor.persistent_undo = PersistentUndoConfig {
        enable: true,
        dir: Some(undo_dir.to_path_buf()),
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
    test_key_sequence(
        &mut app,
        Some("u"),
        Some(&|app| {
            let doc = doc!(app.editor);
            assert_eq!(doc.text().to_string(), "something else\n");
        }),
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
