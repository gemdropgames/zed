use csv_preview::{CsvPreviewView, TabularDataPreviewFeatureFlag};
use editor::{Editor, MultiBuffer};
use feature_flags::FeatureFlagAppExt as _;
use gpui::{AnyElement, Entity};
use markdown_preview::markdown_preview_view::MarkdownPreviewView;
use svg_preview::svg_preview_view::SvgPreviewView;
use ui::{Tooltip, prelude::*};

use super::QuickActionBar;

pub(crate) enum PreviewTarget {
    Markdown(Entity<Editor>),
    Svg(Entity<MultiBuffer>),
    Csv(Entity<Editor>),
}

impl QuickActionBar {
    // Resolves against this toolbar's own pane item rather than the
    // workspace's focused item, so each pane's button reflects and
    // targets the content of the pane it belongs to.
    pub(crate) fn preview_target(&self, cx: &App) -> Option<PreviewTarget> {
        let active_item = self.active_item.as_ref()?;
        let editor = active_item.act_as::<Editor>(cx);

        if let Some(editor) = &editor
            && MarkdownPreviewView::is_markdown_file(editor, cx)
        {
            Some(PreviewTarget::Markdown(editor.clone()))
        } else if let Some(buffer) = active_item.act_as::<MultiBuffer>(cx)
            && SvgPreviewView::is_svg_file(&buffer, cx)
        {
            Some(PreviewTarget::Svg(buffer))
        } else if let Some(editor) = editor
            && cx.has_flag::<TabularDataPreviewFeatureFlag>()
            && CsvPreviewView::is_csv_file(&editor, cx)
        {
            Some(PreviewTarget::Csv(editor))
        } else {
            None
        }
    }

    pub fn render_preview_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let active_item = self.active_item.as_ref()?;
        let preview_target = self.preview_target(cx)?;

        let (button_id, tooltip_text, open_action_for_tooltip) = match &preview_target {
            PreviewTarget::Markdown(_) => (
                "toggle-markdown-preview",
                "Preview Markdown",
                &markdown_preview::OpenPreview as &dyn gpui::Action,
            ),
            PreviewTarget::Svg(_) => (
                "toggle-svg-preview",
                "Preview SVG",
                &svg_preview::OpenPreview as &dyn gpui::Action,
            ),
            PreviewTarget::Csv(_) => (
                "toggle-csv-preview",
                "Preview CSV",
                &csv_preview::OpenPreview as &dyn gpui::Action,
            ),
        };

        let button = IconButton::new(button_id, IconName::Eye)
            .icon_size(IconSize::Small)
            .style(ButtonStyle::Subtle)
            .tooltip(move |_window, cx| {
                Tooltip::for_action(tooltip_text, open_action_for_tooltip, cx)
            })
            .on_click({
                let workspace_handle = self.workspace.clone();
                let active_item = active_item.boxed_clone();
                move |_, window, cx| {
                    let Some(workspace) = workspace_handle.upgrade() else {
                        return;
                    };
                    workspace.update(cx, |workspace, cx| {
                        let Some(pane) = workspace.pane_for(active_item.as_ref()) else {
                            return;
                        };
                        // ZedGG: always open in a split beside the editor
                        // (reusing the adjacent pane when one exists), never
                        // on top of the tab being previewed.
                        match &preview_target {
                            PreviewTarget::Markdown(editor) => {
                                MarkdownPreviewView::open_preview_to_the_side_of_pane(
                                    workspace,
                                    editor.clone(),
                                    pane,
                                    window,
                                    cx,
                                );
                            }
                            PreviewTarget::Svg(buffer) => {
                                SvgPreviewView::open_preview_to_the_side_of_pane(
                                    workspace,
                                    buffer.clone(),
                                    pane,
                                    window,
                                    cx,
                                );
                            }
                            PreviewTarget::Csv(editor) => {
                                CsvPreviewView::open_preview_to_the_side_of_pane(
                                    workspace,
                                    editor.clone(),
                                    pane,
                                    window,
                                    cx,
                                );
                            }
                        }
                    });
                }
            });

        Some(button.into_any_element())
    }
}
