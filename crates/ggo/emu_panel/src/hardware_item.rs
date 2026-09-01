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

use gpui::{App, Context, EventEmitter, FocusHandle, Focusable, SharedString, WeakEntity, Window};
use ui::prelude::*;
use ui::{Banner, CommonAnimationExt, Disclosure, Tooltip};
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};

use std::time::Duration;

use crate::hardware::{FlashProgress, PhaseRow, PhaseState, Remedy, Requirement};
use crate::{EmuPanel, open_emu_item};

pub enum HardwareItemEvent {
    UpdateTab,
}

pub struct HardwareSetupItem {
    panel: WeakEntity<EmuPanel>,
    focus_handle: FocusHandle,
    /// The reader's own choice about the transcript, which beats the
    /// auto-open on failure in both directions.
    log_expanded: Option<bool>,
    /// The reader re-opened the checklist a ready machine collapses.
    requirements_expanded: bool,
}

/// Which parts of the page are showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PageLayout {
    pub requirements_open: bool,
    pub timeline: bool,
    pub log_open: bool,
}

/// The page's one layout rule, kept out of `render` so it can be
/// asserted without a window: a machine that cannot flash gets the
/// checklist, a run gets the timeline, and a failure puts the child's
/// own words on screen without a click.
pub(crate) fn page_layout(
    ready: bool,
    progress: Option<&FlashProgress>,
    log_toggled: Option<bool>,
) -> PageLayout {
    PageLayout {
        requirements_open: !ready,
        // A setup run announces no phases, so its progress has no rows
        // and the page stays on its console.
        timeline: progress.is_some_and(|progress| !progress.rows().is_empty()),
        log_open: log_toggled
            .unwrap_or_else(|| progress.is_some_and(|progress| progress.verdict() == Some(false))),
    }
}

/// What the version-skew banner says.
///
/// Kept out of `render` for the same reason [`page_layout`] is: the
/// wording IS the feature -- a hash pair on its own means nothing to the
/// reader -- so it gets asserted without a window. `can_update` is
/// whether the banner carries its own Update button; when it does not,
/// the banner has to name the step the reader takes instead, or a dev
/// checkout is left with a warning and no way out of it.
///
/// Two hashes have no ordering, but git can say whether the flash repo
/// KNOWS the emulator's commit (`HardwareEnv::emu_commit_in_repo`), and
/// the remedy differs by that answer: a commit the repo has never seen
/// cannot be pulled -- the emulator was built from unpushed work -- while
/// a known commit means the repo has moved past the emulator and ZedGG
/// itself is the stale side. When git could not be asked, the text names
/// both directions rather than confidently prescribing the wrong fix.
pub(crate) fn skew_banner_text(
    flash_short: &str,
    emu_short: &str,
    can_update: bool,
    emu_commit_in_repo: Option<bool>,
) -> String {
    let mut text = format!(
        "The flash repo is at {flash_short} but the emulator was built from {emu_short} \
         -- the board may render differently than the emulator."
    );
    match emu_commit_in_repo {
        Some(false) => {
            text.push_str(
                " The emulator's commit is not in the flash repo's history: it was built \
                 from unpushed work. Push that checkout to the GGO remote, then ",
            );
            text.push_str(if can_update {
                "update the GGO repo here."
            } else {
                "update your checkout."
            });
        }
        Some(true) => {
            text.push_str(
                " The flash repo is ahead of the emulator: updating it cannot help, \
                 ZedGG itself needs rebuilding against it.",
            );
        }
        None => {
            if !can_update {
                text.push_str(" Update your checkout, then flash + rebuild gateware.");
            }
            text.push_str(
                " If updating the GGO repo does not clear this, the checkout is ahead of \
                 the emulator and ZedGG itself needs rebuilding against it.",
            );
        }
    }
    text
}

/// What a flash button on this page promises, given the machine's state
/// and the world the run would boot.
///
/// Kept out of `render` for the same reason [`skew_banner_text`] is: the
/// world named here is the answer to "which world will the board show",
/// which cost a day of debugging when the buttons could not say it, so it
/// gets asserted without a window. A busy or unready button describes
/// what pressing it does INSTEAD, so neither names a world.
pub(crate) fn flash_button_tooltip(
    busy: bool,
    ready: bool,
    base: &str,
    world: Option<&str>,
) -> String {
    if busy {
        return "Stop the run and kill the child process".to_string();
    }
    if !ready {
        return "Still missing something above".to_string();
    }
    ggo_common::flash_tooltip(base, world)
}

/// `m:ss`, the only duration this page shows.
fn elapsed_text(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
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
            log_expanded: None,
            requirements_expanded: false,
        }
    }

    /// One phase of the pipeline: where it got to, for how long, and --
    /// while it is the running one -- what it is doing right now.
    fn render_phase(&self, row: &PhaseRow, now: Duration) -> AnyElement {
        let (name, color) = match row.state {
            PhaseState::Pending => (IconName::Circle, Color::Muted),
            PhaseState::Running => (IconName::ArrowCircle, Color::Accent),
            PhaseState::Done => (IconName::Check, Color::Success),
            PhaseState::Failed => (IconName::XCircle, Color::Error),
        };
        let icon = Icon::new(name).size(IconSize::Small).color(color);
        // The spin is not decoration: it is the only thing repainting
        // this page during a phase that says nothing for minutes, which
        // is also what keeps the elapsed time moving.
        let icon = if row.state == PhaseState::Running {
            icon.with_rotate_animation(2).into_any_element()
        } else {
            icon.into_any_element()
        };
        v_flex()
            .gap_0p5()
            .py_0p5()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(icon)
                    .child(Label::new(row.title.clone()).color(match row.state {
                        PhaseState::Pending => Color::Muted,
                        _ => Color::Default,
                    }))
                    .child(div().flex_1())
                    .when(row.state != PhaseState::Pending, |el| {
                        el.child(
                            Label::new(elapsed_text(row.elapsed(now)))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            )
            .when_some(
                row.detail
                    .clone()
                    .filter(|_| row.state == PhaseState::Running),
                |el, detail| {
                    el.child(
                        div().pl_6().child(
                            Label::new(detail)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .single_line(),
                        ),
                    )
                },
            )
            .into_any_element()
    }

    /// The run: what it is going onto, how far it got, and how it ended.
    fn render_timeline(
        &self,
        progress: &FlashProgress,
        now: Duration,
        target: &(Option<String>, Option<String>),
        cx: &Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let (project, port) = target;
        let verdict = progress.verdict().map(|pass| {
            Label::new(if pass { "PASS" } else { "FAIL" })
                .size(LabelSize::Small)
                .color(if pass { Color::Success } else { Color::Error })
        });
        v_flex()
            .gap_1()
            .p_2()
            .rounded_sm()
            .bg(colors.element_background)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Label::new(match (project, port) {
                            (Some(project), Some(port)) => format!("{project} → {port}"),
                            (Some(project), None) => project.clone(),
                            _ => "this project".to_string(),
                        })
                        .size(LabelSize::Small),
                    )
                    .child(div().flex_1())
                    .children(verdict)
                    .child(
                        Label::new(elapsed_text(now))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .children(
                progress
                    .rows()
                    .iter()
                    .map(|row| self.render_phase(row, now)),
            )
            .when(!progress.diag_steps().is_empty(), |el| {
                el.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            Label::new("Diagnostics")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .children(progress.diag_steps().iter().map(|step| {
                            Label::new(format!("{} {}", step.index, step.status))
                                .size(LabelSize::XSmall)
                                .color(match step.status.as_str() {
                                    "PASS" => Color::Success,
                                    "FAIL" => Color::Error,
                                    _ => Color::Muted,
                                })
                        })),
                )
            })
            .into_any_element()
    }

    /// The skew banner's remedy: pull the clone this fork manages. Only
    /// ever built when [`crate::hardware::HardwareEnv::update_repo_request`]
    /// has one, so a dev checkout never gets a button onto someone else's
    /// working copy.
    fn render_update_button(&self, busy: bool) -> AnyElement {
        Button::new("ggo-hardware-update-repo", "Update GGO repo")
            .disabled(busy)
            .tooltip(Tooltip::text(if busy {
                "A run is using the repo -- wait for it to finish, or cancel it"
            } else {
                "Pull the latest GGO source into the managed clone \
                 (git pull --ff-only), then flash + rebuild gateware to put \
                 the new gateware on the board"
            }))
            .on_click({
                let panel = self.panel.clone();
                move |_, _window, cx| {
                    panel.update(cx, |panel, cx| panel.update_ggo_repo(cx)).ok();
                }
            })
            .into_any_element()
    }

    /// The pull's escape hatch, for a clone a pull cannot fix (diverged
    /// history, corrupt objects): delete the managed clone and clone it
    /// fresh. Built under the same gate as the update button, so it can
    /// never delete a checkout the user maintains.
    fn render_reclone_button(&self, busy: bool) -> AnyElement {
        Button::new("ggo-hardware-force-reclone", "Force reclone GGO repo")
            .disabled(busy)
            .tooltip(Tooltip::text(if busy {
                "A run is using the repo -- wait for it to finish, or cancel it"
            } else {
                "Delete the managed clone and clone it fresh. Use when \
                 Update fails (diverged or corrupt clone); everything in \
                 the managed clone is discarded"
            }))
            .on_click({
                let panel = self.panel.clone();
                move |_, _window, cx| {
                    panel
                        .update(cx, |panel, cx| panel.force_reclone_ggo_repo(cx))
                        .ok();
                }
            })
            .into_any_element()
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
        let (requirements, ready, busy, status, log, progress, target, skew, can_update, world) =
            panel.update(cx, |panel, cx| {
                let env = panel.hardware_env_cached();
                let target = (
                    env.project
                        .as_ref()
                        .and_then(|project| project.file_name())
                        .map(|name| name.to_string_lossy().into_owned()),
                    env.ports.first().cloned(),
                );
                (
                    env.requirements(),
                    env.ready(),
                    panel.is_flashing(),
                    panel.flash_status(),
                    panel.console_lines(),
                    panel
                        .flash_progress()
                        .map(|(progress, elapsed)| (progress.clone(), elapsed)),
                    target,
                    env.version_skew()
                        .map(|(flash, emu)| (flash, emu, env.emu_commit_in_repo)),
                    env.update_repo_request().is_some(),
                    panel.flash_world(cx),
                )
            });
        let layout = page_layout(
            ready,
            progress.as_ref().map(|(progress, _)| progress),
            self.log_expanded,
        );
        let requirements_open = layout.requirements_open || self.requirements_expanded;
        let unmet = requirements.iter().filter(|r| !r.satisfied()).count();
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
                    // Once the machine can flash, the explanation has
                    // done its job and the run is what matters.
                    .when(!ready, |el| {
                        el.child(
                            Label::new(
                                "Flashing packs this project, writes the card image, programs \
                                 the board and boots it. Here is what that needs.",
                            )
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                    }),
            )
            .child(
                v_flex()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Disclosure::new("ggo-hardware-requirements", requirements_open)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.requirements_expanded = !this.requirements_expanded;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Icon::new(if ready {
                                    IconName::Check
                                } else {
                                    IconName::Warning
                                })
                                .size(IconSize::Small)
                                .color(if ready {
                                    Color::Success
                                } else {
                                    Color::Warning
                                }),
                            )
                            .child(
                                Label::new(if ready {
                                    "Ready to flash".to_string()
                                } else {
                                    format!("{unmet} of {} things missing", requirements.len())
                                })
                                .size(LabelSize::Small),
                            ),
                    )
                    .when(requirements_open, |el| {
                        el.children(
                            requirements
                                .iter()
                                .map(|requirement| self.render_requirement(requirement, cx)),
                        )
                    }),
            )
            // Not a requirement row: nothing is missing, and flashing
            // stays enabled. It is the one thing on this page the
            // machine cannot detect as broken -- a board built from the
            // wrong commit boots and runs, it just draws an older frame.
            .when_some(skew, |el, (flash_short, emu_short, emu_commit_in_repo)| {
                el.child(
                    Banner::new()
                        .severity(Severity::Warning)
                        .wrap_content(true)
                        .child(
                            Label::new(skew_banner_text(
                                &flash_short,
                                &emu_short,
                                can_update,
                                emu_commit_in_repo,
                            ))
                            .size(LabelSize::Small),
                        )
                        // The remedy belongs in the warning that asked
                        // for it, not in a button row three sections
                        // down that says nothing about why it is there.
                        .map(|banner| match can_update {
                            true => banner.action_slot(
                                h_flex()
                                    .gap_1()
                                    .child(self.render_update_button(busy))
                                    .child(self.render_reclone_button(busy)),
                            ),
                            false => banner,
                        }),
                )
            })
            .when_some(
                progress.as_ref().filter(|_| layout.timeline),
                |el, (progress, elapsed)| {
                    el.child(self.render_timeline(progress, *elapsed, &target, cx))
                },
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .when(!ready, |el| {
                        el.child(
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
                                        panel.update(cx, |panel, cx| panel.setup_hardware(cx)).ok();
                                    }
                                }),
                        )
                    })
                    .child(
                        Button::new(
                            "ggo-hardware-flash",
                            if busy {
                                "Cancel"
                            } else if progress.is_some() {
                                "Flash again"
                            } else {
                                "Flash now"
                            },
                        )
                        .disabled(!busy && !ready)
                        .tooltip(Tooltip::text(flash_button_tooltip(
                            busy,
                            ready,
                            "Flash this project to the board and run it",
                            world.as_deref(),
                        )))
                        .on_click({
                            let panel = self.panel.clone();
                            move |_, window, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.flash_to_board(window, cx))
                                    .ok();
                            }
                        }),
                    )
                    // A repo pull that changed the PPU/SoC needs a fresh
                    // bitstream; the plain flash reuses the cached one.
                    .when(!busy, |el| {
                        el.child(
                            Button::new("ggo-hardware-flash-full", "Flash + rebuild gateware")
                                .disabled(!ready)
                                .tooltip(Tooltip::text(flash_button_tooltip(
                                    busy,
                                    ready,
                                    "Place-and-route the SoC (~20 min), flash the fresh \
                                     bitstream, then run this project",
                                    world.as_deref(),
                                )))
                                .on_click({
                                    let panel = self.panel.clone();
                                    move |_, window, cx| {
                                        panel
                                            .update(cx, |panel, cx| {
                                                panel.flash_to_board_with(None, true, window, cx)
                                            })
                                            .ok();
                                    }
                                }),
                        )
                    })
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
                    // The timeline already names the running phase; the
                    // status row is what a run without one has.
                    .when(!layout.timeline, |el| {
                        el.children(status.map(|status| {
                            Label::new(status)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                        }))
                    }),
            )
            .when(!log.is_empty(), |el| {
                el.child(
                    v_flex()
                        .gap_1()
                        .p_2()
                        .rounded_sm()
                        .bg(colors.element_background)
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    Disclosure::new("ggo-hardware-log", layout.log_open).on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            let open = this.log_expanded.unwrap_or(false);
                                            this.log_expanded = Some(!open);
                                            cx.notify();
                                        }),
                                    ),
                                )
                                .child(
                                    Label::new(format!("Output ({} lines)", log.len()))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(div().flex_1())
                                .child({
                                    // The whole transcript in one click:
                                    // a failure is something you paste
                                    // somewhere, and a rendered `Label`
                                    // cannot be selected.
                                    let all = log.join("\n");
                                    IconButton::new("ggo-hardware-copy-log", IconName::Copy)
                                        .icon_size(IconSize::XSmall)
                                        .tooltip(Tooltip::text("Copy all output"))
                                        .on_click(move |_, _, cx| {
                                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                                all.clone(),
                                            ));
                                        })
                                }),
                        )
                        // Newest last, the way a terminal reads; the
                        // tail is what a running install is doing now.
                        .when(layout.log_open, |el| {
                            el.children(log.iter().rev().take(200).rev().map(|line| {
                                Label::new(line.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .single_line()
                            }))
                        }),
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
    use crate::hardware::{FlashProgress, HardwareEnv};
    use gpui::TestAppContext;
    use std::time::Duration;

    #[test]
    fn elapsed_is_minutes_and_padded_seconds() {
        assert_eq!(elapsed_text(Duration::from_secs(9)), "0:09");
        assert_eq!(elapsed_text(Duration::from_secs(65)), "1:05");
        assert_eq!(elapsed_text(Duration::from_secs(600)), "10:00");
    }

    /// The flash buttons say which world the board will boot; a button
    /// that would do something else entirely says that instead.
    #[test]
    fn the_flash_buttons_name_the_world_they_will_boot() {
        let base = "Flash this project to the board and run it";
        assert_eq!(
            flash_button_tooltip(false, true, base, Some("worlds/arena")),
            format!("{base} — boots worlds/arena")
        );
        assert_eq!(
            flash_button_tooltip(false, true, base, None),
            base,
            "no world remembered leaves the project's default_world"
        );
        assert_eq!(
            flash_button_tooltip(true, true, base, Some("worlds/arena")),
            "Stop the run and kill the child process",
            "the button cancels while a run is in flight"
        );
        assert_eq!(
            flash_button_tooltip(false, false, base, Some("worlds/arena")),
            "Still missing something above",
            "naming a world a disabled button cannot flash is noise"
        );
    }

    /// A machine that cannot flash yet gets the checklist, not a
    /// timeline of a run that never happened.
    #[test]
    fn a_bare_machine_shows_the_requirements() {
        let layout = page_layout(false, None, None);
        assert_eq!(
            layout,
            PageLayout {
                requirements_open: true,
                timeline: false,
                log_open: false,
            }
        );
    }

    /// Once everything is satisfied the checklist has nothing left to
    /// say, so it collapses out of the way of the button.
    #[test]
    fn a_ready_machine_collapses_the_requirements() {
        let layout = page_layout(true, None, None);
        assert!(!layout.requirements_open, "five green rows are noise");
        assert!(!layout.timeline, "nothing has run yet");
    }

    /// A run in flight is the whole page.
    #[test]
    fn a_run_in_flight_shows_the_timeline() {
        let mut progress = FlashProgress::flash();
        progress.apply("==> Flash board", Duration::from_secs(0));
        let layout = page_layout(true, Some(&progress), None);
        assert!(layout.timeline);
        assert!(!layout.requirements_open);
        assert!(
            !layout.log_open,
            "a healthy run does not need the transcript"
        );
    }

    /// A failure puts the cause on screen without a click: the log is
    /// where the child's own words are.
    #[test]
    fn a_failed_run_opens_the_log_itself() {
        let mut progress = FlashProgress::flash();
        progress.apply("==> Flash board", Duration::from_secs(0));
        progress.apply("RESULT: FAIL", Duration::from_secs(1));
        assert!(page_layout(true, Some(&progress), None).log_open);
    }

    /// ...but the reader closing it wins. Auto-open is a default, not a
    /// decision the page keeps making.
    #[test]
    fn closing_the_log_beats_the_auto_open() {
        let mut progress = FlashProgress::flash();
        progress.apply("RESULT: FAIL", Duration::from_secs(0));
        assert!(!page_layout(true, Some(&progress), Some(false)).log_open);
        assert!(
            page_layout(true, None, Some(true)).log_open,
            "and opening it on a quiet page works too",
        );
    }

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
                assert!(
                    what.contains("dialout"),
                    "the permission trap is named: {what}"
                );
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
            rows.iter()
                .find(|r| r.name == "Board")
                .and_then(|r| r.found.clone()),
            Some("/dev/ttyUSB0".to_string()),
            "the row names the port that will be used"
        );
    }

    /// The banner names both commits and does not scold a reader who has
    /// a button for it; the reader who has none is told what to do.
    #[test]
    fn the_skew_banner_names_both_commits_and_the_way_out() {
        let with_button = skew_banner_text("0123456789", "fedcba9876", true, None);
        assert!(with_button.contains("0123456789") && with_button.contains("fedcba9876"));
        assert!(
            with_button.contains("render differently"),
            "the consequence is the point: {with_button}"
        );
        assert!(
            !with_button.contains("Update your checkout"),
            "the button above is the way out: {with_button}"
        );

        let manual = skew_banner_text("0123456789", "fedcba9876", false, None);
        assert!(
            manual.contains("Update your checkout"),
            "a dev checkout is the user's to move: {manual}"
        );

        // With no answer from git, two hashes do not say which side is
        // ahead -- so the other direction is named rather than left as a
        // dead end.
        for text in [&with_button, &manual] {
            assert!(
                text.contains("ZedGG itself needs rebuilding"),
                "the repo may be the newer side: {text}"
            );
        }
    }

    /// When git HAS said which side is ahead, the banner prescribes the
    /// one remedy that works instead of hedging in both directions.
    #[test]
    fn the_skew_banner_prescribes_by_direction_when_git_answered() {
        // The repo has never seen the emulator's commit: pulling cannot
        // reach unpushed work, so the way out starts with a push.
        let unpushed = skew_banner_text("0123456789", "fedcba9876", true, Some(false));
        assert!(
            unpushed.contains("unpushed work") && unpushed.contains("Push"),
            "an unreachable commit needs a push first: {unpushed}"
        );
        assert!(
            unpushed.contains("update the GGO repo here"),
            "the button above is still the second step: {unpushed}"
        );
        assert!(
            !unpushed.contains("ZedGG itself needs rebuilding"),
            "rebuilding ZedGG cannot reach a commit the repo lacks: {unpushed}"
        );
        let unpushed_manual = skew_banner_text("0123456789", "fedcba9876", false, Some(false));
        assert!(
            unpushed_manual.contains("update your checkout"),
            "a dev checkout is the user's to move: {unpushed_manual}"
        );

        // The repo knows the commit and has moved past it: pulling fixes
        // nothing, ZedGG is the stale side.
        let repo_ahead = skew_banner_text("0123456789", "fedcba9876", true, Some(true));
        assert!(
            repo_ahead.contains("ZedGG itself needs rebuilding"),
            "a repo past the emulator needs a newer ZedGG: {repo_ahead}"
        );
        assert!(
            repo_ahead.contains("updating it cannot help") && !repo_ahead.contains("Push"),
            "no pull or push helps when the repo is ahead: {repo_ahead}"
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
