//! The map editor's SHARED gpui surface: the tool rail, the stamp
//! controls, the tileset strip picker, the terrain editor, the
//! bind-tileset picker and the resize fields.
//!
//! These used to be `MapPanel` methods. They live here because map editing
//! is moving into `ggo_world_panel` (spec 2026-08-29): the world editor
//! renders the same seven tools over the same [`PaintSession`], and two
//! copies of a strip widget would be two places for the stamp indexing to
//! drift. [`paint_session`](crate::paint_session) already holds everything
//! that is not gpui; this module is the layer above it that is still not
//! either host's.
//!
//! **How a host plugs in.** Everything here is generic over a
//! [`PaintHost`] -- a view that can hand out the session under the brush,
//! recompose when the document moves, and own the two gpui entities a
//! session cannot (the resize inputs and the terrain-name input, which are
//! `Entity<Editor>` and so belong to a view). Nothing here reaches into a
//! host's own state, which is what lets the standalone panel and the world
//! panel mount the identical elements.
//!
//! What is NOT here: the map canvas and the camera. Zoom, pan and
//! hit-testing are the two hosts' genuinely different problems -- the
//! standalone panel draws at an integer zoom with the map filling the
//! surface, the world panel draws the map in world space under a float
//! camera alongside entities -- so sharing them would mean one of them
//! pretending to be the other.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use editor::Editor;
use gpui::{
    App, BorderStyle, Bounds, ContentMask, Context, Corners, Entity, Focusable, Hsla, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Render,
    RenderImage, Styled, Window, bounds, div, fill, outline, point, px, size,
};
use ui::prelude::*;
use ui::{ContextMenu, PopoverMenu, Tooltip};

use ggo_worldlib::sprites::io;
use ggo_worldlib::sprites::map_doc::palette_sel_rect;
use ggo_worldlib::sprites::terrain;
use ggo_worldlib::sprites::tileset_doc::TILE_PX;

use crate::geom;
use crate::paint_session::{MapTool, PaintSession};

/// The tileset strip's height. Two rows of 16px tiles at
/// [`geom::STRIP_ZOOM`] plus room to scroll a taller sheet.
const STRIP_HEIGHT: Pixels = px(104.);

/// A view that hosts a [`PaintSession`] and can mount the shared paint
/// widgets over it.
///
/// The required methods are the ones a host answers differently: WHICH
/// session is under the brush (the standalone panel has exactly one, the
/// world panel has one per `.map` it has touched), what "the document
/// changed" means for its own composed image, and where the two editor
/// entities live. Everything a host would otherwise reimplement --
/// binding a tileset, saving terrains, applying a resize -- is provided
/// here so both hosts get one behavior, not two copies of one.
pub trait PaintHost: Render + Sized {
    /// The session under the brush, or `None` when nothing is being
    /// painted (no document open; the world panel's entity mode).
    fn paint_session(&self) -> Option<&PaintSession>;
    fn paint_session_mut(&mut self) -> Option<&mut PaintSession>;
    /// The session's document changed: recompose whatever this host draws
    /// it into. Called only when a mutation actually landed -- a compose
    /// is the expensive step (see [`PaintSession::live_image`]).
    fn paint_session_changed(&mut self, cx: &mut Context<Self>);
    /// The worktree root, which is the frame a tileset's editor sidecar
    /// (and so its terrains) resolves in. `None` outside a project.
    fn paint_project_root(&self) -> Option<PathBuf>;
    /// The resize inputs, once the host has created them.
    fn paint_resize_fields(&self) -> Option<&ResizeFields>;
    /// The terrain editor's name input, once the host has created it.
    fn paint_terrain_name(&self) -> Option<&Entity<Editor>>;

    /// Mutate the session, recompose if the closure reports the DOCUMENT
    /// moved, and repaint either way -- the funnel every widget below
    /// writes through.
    fn update_paint_session(
        &mut self,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut PaintSession) -> bool,
    ) {
        if self.paint_session_mut().is_some_and(edit) {
            self.paint_session_changed(cx);
        }
        cx.notify();
    }

    /// Bind (or rebind) the tileset at asset-root-relative `til_rel` --
    /// the bind picker's pick. [`PaintSession::bind_tileset`] is the
    /// resolve-then-apply rule itself, and its `false` (a binding that
    /// would not open) leaves the document and so the composed pixels
    /// exactly as they were.
    fn bind_paint_tileset(&mut self, til_rel: String, cx: &mut Context<Self>) {
        let project_root = self.paint_project_root();
        self.update_paint_session(cx, |session| {
            session.bind_tileset(til_rel, project_root.as_deref())
        });
    }

    /// Terrain-editor edits all end here: they change the SIDECAR, never
    /// the document, so they repaint without recomposing.
    fn edit_paint_terrains(
        &mut self,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut PaintSession, Option<&std::path::Path>),
    ) {
        let project_root = self.paint_project_root();
        if let Some(session) = self.paint_session_mut() {
            edit(session, project_root.as_deref());
        }
        cx.notify();
    }

    /// Apply the resize fields to the document. Explicit (the button, or
    /// Enter in a field), never on blur: a stray focus change must not
    /// resize the document. Unparsable text is a no-op; an out-of-range
    /// NUMBER clamps ([`geom::parse_dim`]).
    fn apply_paint_resize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(fields) = self.paint_resize_fields().cloned() else {
            return;
        };
        let Some((w, h)) = fields.parsed(cx) else {
            return;
        };
        self.update_paint_session(cx, |session| {
            session.resize(w, h);
            true
        });
        // The clamp may have changed what the user typed, and the field is
        // still focused (they just pressed Enter in it), so `sync` would
        // skip it -- write the APPLIED value back here.
        let applied = self.paint_session().map(|session| {
            let state = session.store.state();
            (state.w, state.h)
        });
        if let Some((w, h)) = applied {
            fields.write_back(w, h, window, cx);
        }
    }
}

// ------------------------------------------------------------- tool rail

/// The seven tools, as toggle buttons. Toolbar-only by design: ggo-ide's
/// map editor has no letter hotkeys for them either.
pub fn render_tool_rail<V: PaintHost>(
    session: &PaintSession,
    cx: &mut Context<V>,
) -> gpui::AnyElement {
    let active = session.tool;
    h_flex()
        .gap_1()
        .flex_wrap()
        .children(MapTool::ALL.map(|tool| {
            IconButton::new(tool.id(), tool.icon())
                .icon_size(IconSize::Small)
                .toggle_state(active == tool)
                .tooltip(Tooltip::text(tool.label()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.update_paint_session(cx, |session| {
                        session.set_tool(tool);
                        false
                    });
                }))
        }))
        .into_any_element()
}

// -------------------------------------------------------- stamp controls

/// The stamp's own state: both flips and the palSub slider. Deliberately
/// NOT the grid toggle or the zoom slider -- those are camera state, and
/// the two hosts' cameras are different animals.
pub fn render_paint_controls<V: PaintHost>(
    session: &PaintSession,
    cx: &mut Context<V>,
) -> gpui::AnyElement {
    let (hflip, vflip, pal_sub) = (session.hflip, session.vflip, session.pal_sub);
    h_flex()
        .gap_1()
        .flex_wrap()
        .child(
            IconButton::new("ggo-map-hflip", IconName::ArrowRightLeft)
                .icon_size(IconSize::Small)
                .toggle_state(hflip)
                .tooltip(Tooltip::text("Flip stamp horizontally"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.update_paint_session(cx, |session| {
                        session.hflip = !session.hflip;
                        false
                    });
                })),
        )
        .child(
            IconButton::new("ggo-map-vflip", IconName::ExpandVertical)
                .icon_size(IconSize::Small)
                .toggle_state(vflip)
                .tooltip(Tooltip::text("Flip stamp vertically"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.update_paint_session(cx, |session| {
                        session.vflip = !session.vflip;
                        false
                    });
                })),
        )
        .child(
            Label::new("pal")
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(Label::new(pal_sub.to_string()).size(LabelSize::XSmall))
        .child(
            ui::Slider::new(
                "ggo-map-pal-sub",
                ui::slider_fraction(
                    pal_sub as usize,
                    geom::PAL_SUB_MIN as usize,
                    geom::PAL_SUB_MAX as usize,
                ),
            )
            .width(px(64.))
            .on_change({
                let weak = cx.weak_entity();
                move |value, _window, cx| {
                    let pal_sub = ui::slider_step(
                        value,
                        geom::PAL_SUB_MIN as usize,
                        geom::PAL_SUB_MAX as usize,
                    ) as u16;
                    weak.update(cx, |this: &mut V, cx| {
                        this.update_paint_session(cx, |session| {
                            session.pal_sub = pal_sub.clamp(geom::PAL_SUB_MIN, geom::PAL_SUB_MAX);
                            false
                        });
                    })
                    .ok();
                }
            }),
        )
        .into_any_element()
}

// ---------------------------------------------------------- strip picker

/// The strip cell under a window-space `position`, gated to tiles the
/// bound tileset actually has (ggo-ide's single `>= tile_count`
/// early-return, shared by the anchor and every dragged-over cell -- a
/// drag into the sheet's zero-filled partial-row padding must not move the
/// selection there). `None` before the first layout, or on a miss.
pub fn strip_cell_at(
    session: &PaintSession,
    strip_bounds: Option<Bounds<Pixels>>,
    position: gpui::Point<Pixels>,
) -> Option<(i32, i32)> {
    let tileset = session.tileset.as_ref()?;
    let strip_bounds = strip_bounds?;
    let local = [
        f32::from(position.x - strip_bounds.origin.x),
        f32::from(position.y - strip_bounds.origin.y),
    ];
    let (c, r) = geom::grid_cell_at(
        local,
        geom::STRIP_ZOOM,
        [0.0, 0.0],
        tileset.cols as u16,
        tileset.rows() as u16,
    )?;
    let cols = tileset.cols.max(1);
    (r as usize * cols + (c as usize) < tileset.tile_count).then_some((c, r))
}

/// The bound tileset's tiles, rect-selectable as a stamp -- or, when the
/// map is UNBOUND, the bind prompt in its place: an unbound map has no
/// tiles to pick from, so what the pane owes the user there is the one
/// action that gets them some (spec 2026-08-29's "bind-tileset prompt").
///
/// `strip_bounds` is the slot the strip's prepaint records its on-screen
/// bounds into, which is what turns a window-space mouse position into a
/// tile ([`strip_cell_at`]). The host owns it because it must outlive a
/// render; nothing here reads it except through the shared `Rc`.
pub fn render_strip<V: PaintHost>(
    session: &PaintSession,
    strip_bounds: &Rc<RefCell<Option<Bounds<Pixels>>>>,
    cx: &mut Context<V>,
) -> gpui::AnyElement {
    let Some(tileset) = &session.tileset else {
        return v_flex()
            .gap_1()
            .p_1()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new(
                    session
                        .tileset_error
                        .clone()
                        .unwrap_or_else(|| "No tileset bound".to_string()),
                )
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .child(render_bind_picker(session, cx))
            .into_any_element();
    };
    let (w, h) =
        geom::grid_pixel_size(tileset.cols as u16, tileset.rows() as u16, geom::STRIP_ZOOM);
    let scene = StripScene {
        image: session.strip.clone(),
        sel: palette_sel_rect(session.pal_anchor, session.pal_far),
        accent: gpui::rgb(0xebcb8b).into(),
        background: cx.theme().colors().editor_background,
    };
    let bounds_slot = strip_bounds.clone();
    let element = gpui::canvas(
        move |canvas_bounds, _window, _cx| {
            *bounds_slot.borrow_mut() = Some(canvas_bounds);
            scene
        },
        move |canvas_bounds, scene, window, _cx| paint_strip(&scene, canvas_bounds, window),
    )
    .w(px(w))
    .h(px(h));

    let for_down = strip_bounds.clone();
    let for_move = strip_bounds.clone();
    // None of the three touches the DOCUMENT -- a strip pick moves the
    // stamp, not the map -- so they notify directly instead of going
    // through `update_paint_session`, and only when something moved: this
    // element sees a move event for every pixel the cursor crosses.
    div()
        .id("ggo-map-strip")
        .h(STRIP_HEIGHT)
        .overflow_scroll()
        .border_t_1()
        .border_color(cx.theme().colors().border)
        .child(element)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                let slot = *for_down.borrow();
                let Some(session) = this.paint_session_mut() else {
                    return;
                };
                let cell = strip_cell_at(session, slot, event.position);
                if session.strip_press(cell) {
                    cx.notify();
                }
            }),
        )
        .on_mouse_move(
            cx.listener(move |this, event: &MouseMoveEvent, _window, cx| {
                let slot = *for_move.borrow();
                let held = event.pressed_button == Some(MouseButton::Left);
                let Some(session) = this.paint_session_mut() else {
                    return;
                };
                if !session.pal_dragging {
                    return;
                }
                let cell = strip_cell_at(session, slot, event.position);
                if session.strip_move(cell, held) {
                    cx.notify();
                }
            }),
        )
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _: &MouseUpEvent, _window, _cx| {
                if let Some(session) = this.paint_session_mut() {
                    session.strip_release();
                }
            }),
        )
        .into_any_element()
}

// ----------------------------------------------------------- bind picker

/// The tileset picker: every `.til` under the session's asset root, one
/// pick away from `MapOp::BindTileset`.
///
/// The candidate walk is LAZY (inside the popover's menu builder) rather
/// than a rendered-in list, for the same reason the world panel's layers
/// rail makes it lazy: it is a recursive walk of the asset root and this
/// element redraws on every notify. It also means a tileset created while
/// the editor is open shows up on the next open of the picker.
///
/// Carries [`PaintSession::tileset_error`] beside it, but only while a
/// tileset IS bound -- that state is a REFUSED REBIND, whose reason has
/// nowhere else to appear. An unbound map's error is the reason it has no
/// strip, so [`render_strip`] shows it there instead of here.
pub fn render_bind_picker<V: PaintHost>(
    session: &PaintSession,
    cx: &mut Context<V>,
) -> gpui::AnyElement {
    let bound = session.store.state().til_path;
    let label = if bound.is_empty() {
        "Bind tileset…".to_string()
    } else {
        bound
    };
    let root = session.root.clone();
    let rebind_error = session
        .tileset
        .is_some()
        .then_some(session.tileset_error.as_ref())
        .flatten();
    let weak = cx.weak_entity();
    let picker = PopoverMenu::new("ggo-map-bind-menu")
        .trigger(Button::new("ggo-map-bind", label).label_size(LabelSize::XSmall))
        .menu(move |window, cx| {
            let tilesets = io::list_tilesets(&root);
            let weak = weak.clone();
            Some(ContextMenu::build(
                window,
                cx,
                move |mut menu, _window, _cx| {
                    for til in tilesets {
                        let weak = weak.clone();
                        menu = menu.entry(
                            SharedString::from(til.clone()),
                            None,
                            move |_window, cx| {
                                let til = til.clone();
                                weak.update(cx, |this: &mut V, cx| {
                                    this.bind_paint_tileset(til, cx)
                                })
                                .ok();
                            },
                        );
                    }
                    menu
                },
            ))
        });
    h_flex()
        .gap_1()
        .flex_wrap()
        .child(picker)
        .children(rebind_error.map(|e| {
            ggo_common::CopyableText::new("ggo-map-tileset-error-copy", e.clone())
                .size(LabelSize::XSmall)
                .color(Color::Muted)
        }))
        .into_any_element()
}

// --------------------------------------------------------- resize fields

/// The two dimension inputs. A `Clone` of it is two `Entity` handles, so
/// a host can hand one out without lending itself out with it -- which is
/// what lets [`PaintHost::apply_paint_resize`] mutate the session and then
/// write the applied dimensions back.
#[derive(Clone)]
pub struct ResizeFields {
    w: Entity<Editor>,
    h: Entity<Editor>,
}

impl ResizeFields {
    pub fn new<V: 'static>(w: u16, h: u16, window: &mut Window, cx: &mut Context<V>) -> Self {
        ResizeFields {
            w: single_line_field(w.to_string(), window, cx),
            h: single_line_field(h.to_string(), window, cx),
        }
    }

    /// Refresh any UNFOCUSED field whose text no longer matches the
    /// document -- which is how an undo/redo of a `Resize`, or the clamp a
    /// resize applied, shows up in the fields. Skipping the focused one is
    /// `ggo_world_panel::ensure_inspector`'s rule and matters for the same
    /// reason: a render must never yank the digits out from under someone
    /// mid-type.
    pub fn sync<V: 'static>(&self, w: u16, h: u16, window: &mut Window, cx: &mut Context<V>) {
        for (editor, value) in [(&self.w, w), (&self.h, h)] {
            if editor.focus_handle(cx).is_focused(window) {
                continue;
            }
            let text = value.to_string();
            if editor.read(cx).text(cx) != text {
                editor.update(cx, |editor, cx| editor.set_text(text, window, cx));
            }
        }
    }

    /// Both fields as legal dimensions, or `None` when either is not a
    /// number at all (an Apply on garbage must leave the document alone
    /// rather than resize it to the minimum).
    pub fn parsed(&self, cx: &App) -> Option<(u16, u16)> {
        let w = geom::parse_dim(&self.w.read(cx).text(cx))?;
        let h = geom::parse_dim(&self.h.read(cx).text(cx))?;
        Some((w, h))
    }

    /// The two editor entities, for a test that types into them.
    #[cfg(any(test, feature = "test-support"))]
    pub fn editors(&self) -> (Entity<Editor>, Entity<Editor>) {
        (self.w.clone(), self.h.clone())
    }

    pub fn write_back<V: 'static>(&self, w: u16, h: u16, window: &mut Window, cx: &mut Context<V>) {
        for (editor, value) in [(&self.w, w), (&self.h, h)] {
            editor.update(cx, |editor, cx| {
                editor.set_text(value.to_string(), window, cx)
            });
        }
    }
}

fn single_line_field<V: 'static>(
    text: String,
    window: &mut Window,
    cx: &mut Context<V>,
) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_text(text, window, cx);
        editor
    })
}

/// Create the resize fields, or refresh the ones that exist. Returns the
/// fields to store when it had to make them -- the host owns them, since
/// they have to outlive a render.
pub fn ensure_resize_fields<V: 'static>(
    existing: Option<&ResizeFields>,
    w: u16,
    h: u16,
    window: &mut Window,
    cx: &mut Context<V>,
) -> Option<ResizeFields> {
    match existing {
        Some(fields) => {
            fields.sync(w, h, window, cx);
            None
        }
        None => Some(ResizeFields::new(w, h, window, cx)),
    }
}

/// The `W`/`H` inputs plus their Apply button.
pub fn render_resize<V: PaintHost>(fields: &ResizeFields, cx: &mut Context<V>) -> gpui::AnyElement {
    h_flex()
        .gap_1()
        .items_center()
        .child(size_field("W", &fields.w, cx))
        .child(size_field("H", &fields.h, cx))
        .child(
            Button::new("ggo-map-resize", "Resize")
                .label_size(LabelSize::XSmall)
                .on_click(cx.listener(|this, _, window, cx| this.apply_paint_resize(window, cx))),
        )
        .into_any_element()
}

/// One labelled resize input, in the minimal bordered box the world
/// panel's inspector fields use (primitive gpui/ui components only -- no
/// widget framework).
fn size_field<V: 'static>(
    label: &str,
    editor: &Entity<Editor>,
    cx: &Context<V>,
) -> gpui::AnyElement {
    h_flex()
        .gap_0p5()
        .items_center()
        .child(
            Label::new(label.to_string())
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(
            div()
                .w(px(44.))
                .px_1()
                .border_1()
                .border_color(cx.theme().colors().border_variant)
                .rounded_sm()
                .child(editor.clone()),
        )
        .into_any_element()
}

// -------------------------------------------------------- terrain editor

/// The autotile terrain editor, shown while the Terrain tool is active:
/// the terrain list with add/rename/remove, the 3x3 neighbour pad that
/// drafts a mask, and the selected terrain's labelled tiles.
///
/// `None` under any other tool -- it is a lot of surface to keep on screen
/// for a brush that will not use it.
///
/// `name_editor` is passed rather than read back off the host because the
/// host is LEASED for the duration of its own render; the value comes off
/// [`PaintHost::paint_terrain_name`] at the call site, where it is still
/// reachable.
pub fn render_terrain_editor<V: PaintHost>(
    session: &PaintSession,
    name_editor: Option<&Entity<Editor>>,
    cx: &mut Context<V>,
) -> Option<gpui::AnyElement> {
    if session.tool != MapTool::Terrain {
        return None;
    }
    let selected = session.terrain.and_then(|i| session.terrains.get(i));
    let anchor = session.anchor_tile();
    let mask = session.mask_draft;
    let name_field = name_editor.map(|editor| {
        div()
            .w(px(120.))
            .px_1()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .rounded_sm()
            .child(editor.clone())
    });
    // Rows of the 3x3 neighbour pad; the centre is the anchor tile.
    let pad = [
        [
            Some(terrain::NORTH_WEST),
            Some(terrain::NORTH),
            Some(terrain::NORTH_EAST),
        ],
        [Some(terrain::WEST), None, Some(terrain::EAST)],
        [
            Some(terrain::SOUTH_WEST),
            Some(terrain::SOUTH),
            Some(terrain::SOUTH_EAST),
        ],
    ];
    Some(
        v_flex()
            .gap_1()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .children(name_field)
                    .child(
                        Button::new("ggo-map-terrain-add", "Add")
                            .disabled(session.tileset.is_none())
                            .on_click(cx.listener(|this, _, _, cx| {
                                let name = terrain_name_text(this, cx);
                                this.edit_paint_terrains(cx, |session, root| {
                                    session.add_terrain(name, root)
                                });
                            })),
                    )
                    .child(
                        Button::new("ggo-map-terrain-rename", "Rename")
                            .disabled(selected.is_none())
                            .on_click(cx.listener(|this, _, _, cx| {
                                let name = terrain_name_text(this, cx);
                                this.edit_paint_terrains(cx, |session, root| {
                                    session.rename_terrain(name, root)
                                });
                            })),
                    )
                    .child(
                        Button::new("ggo-map-terrain-remove", "Remove")
                            .disabled(selected.is_none())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.edit_paint_terrains(cx, PaintSession::remove_terrain);
                            })),
                    )
                    .children(session.terrains.iter().enumerate().map(|(i, t)| {
                        Button::new(("ggo-map-terrain", i), t.name.clone())
                            .toggle_state(session.terrain == Some(i))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.update_paint_session(cx, |session| {
                                    session.select_terrain(i);
                                    false
                                });
                            }))
                    })),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(v_flex().children(pad.iter().enumerate().map(|(row, bits)| {
                        h_flex().children(bits.iter().enumerate().map(|(col, bit)| {
                            let id = ("ggo-map-mask", row * 3 + col);
                            match bit {
                                Some(bit) => {
                                    let bit = *bit;
                                    Button::new(id, if mask & bit != 0 { "■" } else { "·" })
                                        .toggle_state(mask & bit != 0)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.update_paint_session(cx, |session| {
                                                session.mask_draft ^= bit;
                                                false
                                            });
                                        }))
                                }
                                None => Button::new(
                                    id,
                                    anchor.map_or("—".to_string(), |t| format!("T{t}")),
                                )
                                .disabled(true),
                            }
                        }))
                    })))
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                Button::new("ggo-map-terrain-assign", "Assign")
                                    .disabled(selected.is_none() || anchor.is_none())
                                    .tooltip(Tooltip::text(
                                        "Give the stamp's first tile this neighbour mask",
                                    ))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.edit_paint_terrains(
                                            cx,
                                            PaintSession::assign_anchor_tile,
                                        );
                                    })),
                            )
                            .child(
                                Label::new(terrain::mask_glyphs(terrain::canonical(mask)))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(h_flex().gap_1().flex_wrap().children(
                        selected.into_iter().flat_map(|t| t.tiles.iter()).map(|tt| {
                            let tile = tt.tile;
                            h_flex()
                                .gap_0p5()
                                .child(
                                    Label::new(format!(
                                        "T{} {}",
                                        tile,
                                        terrain::mask_glyphs(tt.mask)
                                    ))
                                    .size(LabelSize::XSmall),
                                )
                                .child(
                                    IconButton::new(
                                        ("ggo-map-terrain-tile", tile as usize),
                                        IconName::Close,
                                    )
                                    .icon_size(IconSize::XSmall)
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.edit_paint_terrains(cx, |session, root| {
                                                session.unassign_tile(tile, root)
                                            });
                                        },
                                    )),
                                )
                        }),
                    )),
            )
            .children(session.terrain_error.as_ref().map(|e| {
                Label::new(e.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Warning)
            }))
            .into_any_element(),
    )
}

/// The terrain name input's trimmed text -- what Add and Rename name a
/// terrain. Empty (and so refused by the session) when the host has not
/// created the input yet.
fn terrain_name_text<V: PaintHost>(host: &V, cx: &App) -> String {
    host.paint_terrain_name()
        .map(|editor| editor.read(cx).text(cx).trim().to_string())
        .unwrap_or_default()
}

// --------------------------------------------------------------- painting

/// Everything the strip's paint closure needs, captured at render time.
struct StripScene {
    image: Option<Arc<RenderImage>>,
    sel: (i32, i32, i32, i32),
    accent: Hsla,
    background: Hsla,
}

/// Rect covering cells `[c0..=c1] x [r0..=r1]` in canvas space, for a grid
/// drawn at integer `zoom` with its top-left at `pan`.
pub(crate) fn cell_rect(
    canvas: Bounds<Pixels>,
    pan: [f32; 2],
    zoom: usize,
    c0: i32,
    r0: i32,
    c1: i32,
    r1: i32,
) -> Bounds<Pixels> {
    let step = (TILE_PX * zoom.max(1)) as f32;
    let x = canvas.origin.x + px(pan[0] + c0 as f32 * step);
    let y = canvas.origin.y + px(pan[1] + r0 as f32 * step);
    bounds(
        point(x, y),
        size(
            px((c1 - c0 + 1) as f32 * step),
            px((r1 - r0 + 1) as f32 * step),
        ),
    )
}

fn paint_strip(scene: &StripScene, canvas: Bounds<Pixels>, window: &mut Window) {
    window.with_content_mask(Some(ContentMask { bounds: canvas }), |window| {
        window.paint_quad(fill(canvas, scene.background));
        if let Some(image) = &scene.image {
            // Discarded like every other per-frame `paint_image` in the
            // fork (`ggo_world_panel::canvas::paint_item`): a failure here
            // would recur on every frame, so logging it would bury the log
            // rather than inform anyone.
            let _ = window.paint_image(
                canvas,
                canvas,
                Corners::default(),
                image.clone(),
                0,
                false,
                true,
            );
        }
        let (c0, r0, c1, r1) = scene.sel;
        let r = cell_rect(canvas, [0.0, 0.0], geom::STRIP_ZOOM, c0, r0, c1, r1);
        window.paint_quad(outline(r, scene.accent, BorderStyle::default()));
    });
}
