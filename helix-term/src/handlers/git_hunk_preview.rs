use helix_event::register_hook;
use helix_view::document::Mode;

use crate::events::{OnModeSwitch, PostCommand};
use crate::handlers::Handlers;
use crate::ui::{self, Popup};
use crate::{commands::typed, keymap::MappableCommand};

fn sticky_git_hunk_preview_post_command(
    PostCommand { command, cx }: &mut PostCommand<'_, '_>,
) -> anyhow::Result<()> {
    if matches!(
        command,
        MappableCommand::Static {
            name: "command_mode",
            ..
        }
    ) {
        return Ok(());
    }

    cx.callback.push(Box::new(|compositor, ctx| {
        if compositor
            .find_id::<Popup<ui::Markdown>>(typed::GIT_HUNK_PREVIEW_ID)
            .is_some()
        {
            typed::refresh_git_hunk_preview(ctx.editor, compositor);
        }
    }));

    Ok(())
}

pub(super) fn register_hooks(_handlers: &Handlers) {
    register_hook!(move |event: &mut PostCommand<'_, '_>| {
        sticky_git_hunk_preview_post_command(event)
    });

    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
        if event.new_mode != Mode::Normal {
            event.cx.callback.push(Box::new(|compositor, _| {
                compositor.remove(typed::GIT_HUNK_PREVIEW_ID);
            }));
        }
        Ok(())
    });
}
