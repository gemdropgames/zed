//! The hardware setup page: a center tab that says what flashing needs,
//! what this machine has, and installs the rest.
//!
//! Pressing flash with something missing used to write one long sentence
//! onto the emulator's status row -- unreadable, and with no way to act
//! on it. This is that sentence turned into a page with buttons.
//!
//! It is a VIEW over [`EmuPanel`], not a second engine: the probe, the
//! install steps, the streaming runner and the console all already live
//! there and are tested there. This file renders them and nothing else.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, SharedString, WeakEntity, Window,
};
use ui::prelude::*;
use ui::Tooltip;
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};

use crate::hardware::{Remedy, Requirement};
use crate::{EmuPanel, open_emu_item};

pub enum HardwareItemEvent {
    UpdateTab,
}

pub struct HardwareSetupItem {
    panel: WeakEntity<EmuPanel>,
    focus_handle: FocusHandle,
}

impl HardwareSetupItem {
    pub fn new(panel: WeakEntity<EmuPanel>, cx: &mut Context<Self>) -> Self {
        if let Some(panel) = panel.upgrade() {
            // The emulator's state IS this page's state: a landed install
            // or a finished probe has to repaint the rows.
            cx.observe(&panel, |_, _, cx| cx.emit(HardwareItemEvent::UpdateTab))
                .detach();
        }
        Self {
            panel,
            focus_handle: cx.focus_handle(),
        }
    }

    /// One requirement's row: state, where it was found or why not, and
    /// what will be done about it.
    fn render_requirement(&self, requirement: &Requirement, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let (icon, color) = if requirement.satisfied() {
            (IconName::Check, Color::Success)
        } else if matches!(requirement.remedy, Remedy::Install(_)) {
            (IconName::Download, Color::Warning)
        } else {
            (IconName::Warning, Color::Error)
        };
        let detail = match (&requirement.found, &requirement.remedy) {
            (Some(found), _) => found.clone(),
            (None, Remedy::Install(what)) => format!("ZedGG will run: {what}"),
            (None, Remedy::Manual(what)) => what.clone(),
            (None, Remedy::Satisfied) => requirement.why.to_string(),
        };
        h_flex()
            .gap_2()
            .py_1()
            .items_start()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(Icon::new(icon).size(IconSize::Small).color(color))
            .child(
                v_flex()
                    .gap_0p5()
                    .flex_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new(requirement.name.to_string()))
                            .child(
                                Label::new(requirement.why.to_string())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(
                        ggo_common::CopyableText::new(
                            SharedString::from(format!("ggo-hardware-detail-{}", requirement.name)),
                            detail,
                        )
                        .size(LabelSize::XSmall)
                        .color(if requirement.satisfied() {
                            Color::Muted
                        } else {
                            Color::Default
                        }),
                    ),
            )
            .into_any_element()
    }
}

impl EventEmitter<HardwareItemEvent> for HardwareSetupItem {}

impl Focusable for HardwareSetupItem {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for HardwareSetupItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(panel) = self.panel.upgrade() else {
            return v_flex()
                .id("ggo-hardware-setup")
                .p_4()
                .child(Label::new("The emulator pane is gone.").color(Color::Muted));
        };
        let (requirements, ready, busy, status, log) = panel.update(cx, |panel, _cx| {
            let env = panel.hardware_env_cached();
            (
                env.requirements(),
                env.ready(),
                panel.is_flashing(),
                panel.flash_status(),
                panel.console_lines(),
            )
        });
        let installable = requirements
            .iter()
            .filter(|r| matches!(r.remedy, Remedy::Install(_)))
            .count();
        let colors = cx.theme().colors();

        v_flex()
            .id("ggo-hardware-setup")
            .size_full()
            .p_4()
            .gap_3()
            .overflow_y_scroll()
            .track_focus(&self.focus_handle)
            .bg(colors.editor_background)
            .child(
                v_flex()
                    .gap_1()
                    .child(Headline::new("Flash to hardware").size(HeadlineSize::Small))
                    .child(
                        Label::new(
                            "Flashing packs this project, writes the card image, programs the \
                             board and boots it. Here is what that needs.",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .child(
                v_flex()
                    .children(
                        requirements
                            .iter()
                            .map(|requirement| self.render_requirement(requirement, cx)),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("ggo-hardware-install", "Install tools")
                            .disabled(busy || installable == 0)
                            .tooltip(Tooltip::text(if installable == 0 {
                                "Nothing left for ZedGG to install"
                            } else {
                                "Clone the GGO repo and install the missing binaries"
                            }))
                            .on_click({
                                let panel = self.panel.clone();
                                move |_, _window, cx| {
                                    panel
                                        .update(cx, |panel, cx| panel.setup_hardware(cx))
                                        .ok();
                                }
                            }),
                    )
                    .child(
                        Button::new("ggo-hardware-flash", "Flash now")
                            .disabled(busy || !ready)
                            .tooltip(Tooltip::text(if ready {
                                "Flash this project to the board and run it"
                            } else {
                                "Still missing something above"
                            }))
                            .on_click({
                                let panel = self.panel.clone();
                                move |_, window, cx| {
                                    panel
                                        .update(cx, |panel, cx| panel.flash_to_board(window, cx))
                                        .ok();
                                }
                            }),
                    )
                    .child(
                        Button::new("ggo-hardware-recheck", "Re-check")
                            .disabled(busy)
                            .tooltip(Tooltip::text("Probe this machine again"))
                            .on_click({
                                let panel = self.panel.clone();
                                move |_, _window, cx| {
                                    panel
                                        .update(cx, |panel, cx| {
                                            panel.invalidate_hardware();
                                            cx.notify();
                                        })
                                        .ok();
                                }
                            }),
                    )
                    .children(status.map(|status| {
                        Label::new(status).size(LabelSize::Small).color(Color::Muted)
                    })),
            )
            .when(!log.is_empty(), |el| {
                el.child(
                    v_flex()
                        .gap_1()
                        .p_2()
                        .rounded_sm()
                        .bg(colors.element_background)
                        .child(
                            Label::new("Output")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        // Newest last, the way a terminal reads; the tail
                        // is what a running install is doing right now.
                        .children(log.iter().rev().take(200).rev().map(|line| {
                            Label::new(line.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .single_line()
                        })),
                )
            })
    }
}

impl Item for HardwareSetupItem {
    type Event = HardwareItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            HardwareItemEvent::UpdateTab => f(ItemEvent::UpdateTab),
        }
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "GGO Hardware".into()
    }
}

/// Activate the workspace's hardware page, or open it. One per workspace
/// (the emulator's shape): it describes the machine, not a document.
pub fn open_hardware_item(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace.items_of_type::<HardwareSetupItem>(cx).next();
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    // The page is a view over the emulator pane, so there has to be one.
    let mut panel = None;
    open_emu_item(workspace, window, cx, |emu, _window, cx| {
        panel = Some(cx.weak_entity());
        emu.invalidate_hardware();
    });
    let Some(panel) = panel else {
        return;
    };
    let item = cx.new(|cx| HardwareSetupItem::new(panel, cx));
    workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::HardwareEnv;
    use gpui::TestAppContext;

    /// The rows a bare machine shows: what is needed, what ZedGG can
    /// install, and what only the user can do.
    #[test]
    fn requirements_separate_installable_gaps_from_manual_ones() {
        let bare = HardwareEnv {
            cargo: true,
            git: true,
            clone_dest: std::path::PathBuf::from("/home/u/.ggo/ggo"),
            home: std::path::PathBuf::from("/home/u"),
            ..Default::default()
        };
        let rows = bare.requirements();
        assert_eq!(rows.len(), 5, "project, repo, ggo-diag, emd, board");
        assert!(rows.iter().all(|r| !r.satisfied()));

        let installable: Vec<&str> = rows
            .iter()
            .filter(|r| matches!(r.remedy, Remedy::Install(_)))
            .map(|r| r.name)
            .collect();
        assert_eq!(
            installable,
            vec!["GGO repo", "ggo-diag", "emd"],
            "the three ZedGG can fix"
        );

        let board = rows.last().expect("the board row is last");
        assert_eq!(board.name, "Board");
        match &board.remedy {
            Remedy::Manual(what) => {
                assert!(what.contains("dialout"), "the permission trap is named: {what}");
                assert!(what.contains("connect the board"), "{what}");
            }
            other => panic!("a board cannot be installed: {other:?}"),
        }

        // No cargo and no git: the same gaps, but now the user's to fix.
        let no_tools = HardwareEnv {
            cargo: false,
            git: false,
            ..bare
        };
        assert!(
            no_tools
                .requirements()
                .iter()
                .all(|r| !matches!(r.remedy, Remedy::Install(_))),
            "nothing is installable without cargo or git"
        );
    }

    /// A satisfied requirement shows where it was found, and nothing is
    /// offered for it.
    #[test]
    fn a_satisfied_requirement_reports_where_it_is() {
        let env = HardwareEnv {
            diag_bin: Some("ggo-diag".into()),
            emd_bin: Some("emd".into()),
            repo: Some(std::path::PathBuf::from("/repo")),
            ports: vec!["/dev/ttyUSB0".into()],
            project: Some(std::path::PathBuf::from("/game")),
            cargo: true,
            git: true,
            ..Default::default()
        };
        assert!(env.ready());
        let rows = env.requirements();
        assert!(rows.iter().all(|r| r.satisfied()));
        assert!(rows.iter().all(|r| r.remedy == Remedy::Satisfied));
        assert_eq!(
            rows.iter().find(|r| r.name == "Board").and_then(|r| r.found.clone()),
            Some("/dev/ttyUSB0".to_string()),
            "the row names the port that will be used"
        );
    }

    /// One page per workspace, and it opens the emulator pane it views.
    #[gpui::test]
    async fn test_the_hardware_page_is_a_singleton_tab(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (workspace, _panel, _worktree_id, cx) =
            crate::tests::run_menu_workspace(cx, dir.path()).await;

        workspace.update_in(cx, |workspace, window, cx| {
            open_hardware_item(workspace, window, cx);
            open_hardware_item(workspace, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.items_of_type::<HardwareSetupItem>(cx).count(),
                1,
                "one page, re-activated"
            );
            let item = workspace
                .items_of_type::<HardwareSetupItem>(cx)
                .next()
                .expect("the page");
            assert_eq!(
                workspace::item::Item::tab_content_text(item.read(cx), 0, cx).as_ref(),
                "GGO Hardware"
            );
        });
    }
}
