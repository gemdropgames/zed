//! Reusable palette-editing widgets over a 16-slot RGB565 palette.
//!
//! [`SmallPaletteEditor`] is the compact form factor for side-dock /
//! tooling-column contexts: the swatch grid plus per-channel steppers for
//! the selected slot. A dedicated full-page palette view would be its own
//! sibling widget here (larger swatches, direct hex entry) -- the pure
//! RGB565 channel helpers below are the shared substrate either builds on.
//!
//! The widget is presentation-only: it owns no document state and applies
//! no ops. Hosts feed it the palette + selection and receive intents
//! through `on_select` / `on_change` callbacks (the tileset editor turns
//! `on_change` into an undoable `TilesetOp::SetPalette`).

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    App, Bounds, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Window, canvas, div, px, rgb,
    rgba,
};
use ui::prelude::*;

use ggo_worldlib::sprites::palette565::{PAL_SLOTS, Pal, slot_rgba};

/// Swatch box (px, square).
const SWATCH_PX: f32 = 18.0;

// ------------------------------------------------------- RGB565 channels

/// The 5-bit red channel of an RGB565 color (0..=31).
pub fn r5(color: u16) -> u16 {
    (color >> 11) & 0x1F
}

/// The 6-bit green channel of an RGB565 color (0..=63).
pub fn g6(color: u16) -> u16 {
    (color >> 5) & 0x3F
}

/// The 5-bit blue channel of an RGB565 color (0..=31).
pub fn b5(color: u16) -> u16 {
    color & 0x1F
}

/// `color` with its red channel replaced by `value` (clamped to 5 bits).
pub fn with_r5(color: u16, value: i32) -> u16 {
    let v = value.clamp(0, 0x1F) as u16;
    (color & !(0x1F << 11)) | (v << 11)
}

/// `color` with its green channel replaced by `value` (clamped to 6 bits).
pub fn with_g6(color: u16, value: i32) -> u16 {
    let v = value.clamp(0, 0x3F) as u16;
    (color & !(0x3F << 5)) | (v << 5)
}

/// `color` with its blue channel replaced by `value` (clamped to 5 bits).
pub fn with_b5(color: u16, value: i32) -> u16 {
    let v = value.clamp(0, 0x1F) as u16;
    (color & !0x1F) | v
}

/// The channel value a click at horizontal fraction `frac` of the slider
/// track means, rounded to the nearest step and clamped to `0..=max`.
pub fn value_at_fraction(frac: f32, max: u16) -> i32 {
    (frac.clamp(0.0, 1.0) * max as f32).round() as i32
}

// --------------------------------------------------------------- widget

type SelectHandler = Rc<dyn Fn(usize, &mut Window, &mut App)>;
type ChangeHandler = Rc<dyn Fn(usize, u16, &mut Window, &mut App)>;

/// Compact palette editor for tooling columns: the 16-swatch grid
/// (click to select, pointer cursor) and, for the selected slot,
/// R/G/B channel steppers plus the hex value. Slot 0 is the transparent
/// index (PPU contract §1) and is selectable but locked against editing.
#[derive(IntoElement)]
pub struct SmallPaletteEditor {
    palette: Pal,
    selected: usize,
    note: Option<SharedString>,
    on_select: Option<SelectHandler>,
    on_change: Option<ChangeHandler>,
}

impl SmallPaletteEditor {
    pub fn new(palette: Pal, selected: usize) -> Self {
        Self {
            palette,
            selected,
            note: None,
            on_select: None,
            on_change: None,
        }
    }

    /// A muted line under the "Palette" heading (e.g. the missing-.pal
    /// fallback warning).
    pub fn note(mut self, note: impl Into<SharedString>) -> Self {
        self.note = Some(note.into());
        self
    }

    pub fn on_select(mut self, handler: impl Fn(usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(handler));
        self
    }

    pub fn on_change(
        mut self,
        handler: impl Fn(usize, u16, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_change = Some(Rc::new(handler));
        self
    }

    /// One `name [====|  ] [-] value [+]` row for a channel of the
    /// selected slot's color: a click/drag-to-set slider track, then the
    /// numeric value with fine-step buttons.
    fn channel_row(
        &self,
        name: &'static str,
        value: u16,
        max: u16,
        apply: fn(u16, i32) -> u16,
        cx: &mut App,
    ) -> gpui::AnyElement {
        let slot = self.selected;
        let color = self.palette[slot];
        let on_change_minus = self.on_change.clone();
        let on_change_plus = self.on_change.clone();
        let on_change_track = self.on_change.clone();
        let on_change_drag = self.on_change.clone();
        // Recorded fresh every frame by the canvas prepaint below;
        // prepaint runs before this frame's mouse events dispatch, so the
        // listeners always see the current track geometry (the same
        // bounds-recording idiom as the sheet canvas, scoped per render
        // because a RenderOnce widget owns no persistent state).
        let track_bounds: Rc<RefCell<Option<Bounds<Pixels>>>> = Rc::new(RefCell::new(None));
        let bounds_for_prepaint = track_bounds.clone();
        let bounds_for_down = track_bounds.clone();
        let bounds_for_drag = track_bounds;
        let value_from = move |pos_x: Pixels, bounds: &Bounds<Pixels>| {
            let width = f32::from(bounds.size.width).max(1.0);
            value_at_fraction(f32::from(pos_x - bounds.origin.x) / width, max)
        };
        let fill = cx.theme().colors().border_focused;
        let track_bg = cx.theme().colors().element_background;
        let frac = value as f32 / max as f32;
        h_flex()
            .gap_1()
            .items_center()
            .child(Label::new(name).size(LabelSize::XSmall).color(Color::Muted))
            .child(
                div()
                    .id(("ggo-pal-slider", slot * 8 + name.len()))
                    .flex_1()
                    .h(px(10.))
                    .rounded_sm()
                    .bg(track_bg)
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .cursor_pointer()
                    .child(div().h_full().w(relative(frac)).rounded_sm().bg(fill))
                    .child(
                        canvas(
                            move |bounds, _, _| {
                                *bounds_for_prepaint.borrow_mut() = Some(bounds);
                            },
                            |_, (), _, _| {},
                        )
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        move |event: &MouseDownEvent, window, cx| {
                            if let (Some(handler), Some(bounds)) =
                                (&on_change_track, *bounds_for_down.borrow())
                            {
                                let next = value_from(event.position.x, &bounds);
                                handler(slot, apply(color, next), window, cx);
                            }
                        },
                    )
                    .on_mouse_move(move |event: &MouseMoveEvent, window, cx| {
                        if event.pressed_button != Some(MouseButton::Left) {
                            return;
                        }
                        if let (Some(handler), Some(bounds)) =
                            (&on_change_drag, *bounds_for_drag.borrow())
                        {
                            let next = value_from(event.position.x, &bounds);
                            handler(slot, apply(color, next), window, cx);
                        }
                    }),
            )
            .child(
                IconButton::new(("ggo-pal-minus", slot * 8 + name.len()), IconName::Dash)
                    .icon_size(IconSize::XSmall)
                    .disabled(value == 0)
                    .on_click(move |_, window, cx| {
                        if let Some(handler) = &on_change_minus {
                            handler(slot, apply(color, value as i32 - 1), window, cx);
                        }
                    }),
            )
            .child(Label::new(format!("{value}")).size(LabelSize::XSmall))
            .child(
                IconButton::new(("ggo-pal-plus", slot * 8 + name.len()), IconName::Plus)
                    .icon_size(IconSize::XSmall)
                    .disabled(value >= max)
                    .on_click(move |_, window, cx| {
                        if let Some(handler) = &on_change_plus {
                            handler(slot, apply(color, value as i32 + 1), window, cx);
                        }
                    }),
            )
            .into_any_element()
    }
}

impl RenderOnce for SmallPaletteEditor {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let accent = cx.theme().colors().border_focused;
        let border = cx.theme().colors().border;
        let selected = self.selected;
        let palette = self.palette;
        let color = palette[selected.min(PAL_SLOTS - 1)];
        v_flex()
            .gap_0p5()
            .p_1()
            .child(
                Label::new("Palette")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .when_some(self.note.clone(), |this, note| {
                this.child(Label::new(note).size(LabelSize::XSmall).color(Color::Muted))
            })
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_0p5()
                    .children((0..PAL_SLOTS).map(|slot| {
                        let [r, g, b, a] = slot_rgba(&palette, slot as u8);
                        let fill = u32::from_be_bytes([0, r, g, b]);
                        let on_select = self.on_select.clone();
                        div()
                            .id(("ggo-pal-swatch", slot))
                            .w(px(SWATCH_PX))
                            .h(px(SWATCH_PX))
                            .border_2()
                            .border_color(if slot == selected { accent } else { border })
                            .rounded_sm()
                            .cursor_pointer()
                            // Slot 0 is transparent: show the panel through
                            // it instead of painting a misleading color.
                            .when(a != 0, |el| el.bg(rgb(fill)))
                            .when(a == 0, |el| el.bg(rgba(0x00000000)))
                            .tooltip(ui::Tooltip::text(format!(
                                "{slot}: #{:04X}{}",
                                palette[slot],
                                if a == 0 { " (transparent)" } else { "" }
                            )))
                            .on_click(move |_, window, cx| {
                                if let Some(handler) = &on_select {
                                    handler(slot, window, cx);
                                }
                            })
                    })),
            )
            .map(|this| {
                if selected == 0 {
                    this.child(
                        Label::new("Slot 0 is the transparent index (locked)")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                } else {
                    this.child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                Label::new(format!("Slot {selected}"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(div().flex_1())
                            .child(
                                Label::new(format!("#{color:04X}"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(self.channel_row("R", r5(color), 0x1F, with_r5, cx))
                    .child(self.channel_row("G", g6(color), 0x3F, with_g6, cx))
                    .child(self.channel_row("B", b5(color), 0x1F, with_b5, cx))
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The channel helpers round-trip every field of a mixed color, clamp
    /// out-of-range writes at both ends, and leave the other channels
    /// untouched.
    #[test]
    fn rgb565_channel_helpers_extract_replace_and_clamp() {
        // 0b10101_010101_01010
        let color: u16 = (0x15 << 11) | (0x15 << 5) | 0x0A;
        assert_eq!(r5(color), 0x15);
        assert_eq!(g6(color), 0x15);
        assert_eq!(b5(color), 0x0A);

        assert_eq!(r5(with_r5(color, 0x1F)), 0x1F);
        assert_eq!(g6(with_g6(color, 0x3F)), 0x3F);
        assert_eq!(b5(with_b5(color, 0x1F)), 0x1F);
        // Replacing one channel leaves the others alone.
        let red_maxed = with_r5(color, 0x1F);
        assert_eq!(g6(red_maxed), 0x15);
        assert_eq!(b5(red_maxed), 0x0A);

        assert_eq!(r5(with_r5(color, -5)), 0);
        assert_eq!(g6(with_g6(color, 999)), 0x3F);
        assert_eq!(b5(with_b5(color, -1)), 0);

        assert_eq!(with_r5(0, 0x1F), 0xF800);
        assert_eq!(with_g6(0, 0x3F), 0x07E0);
        assert_eq!(with_b5(0, 0x1F), 0x001F);
    }

    /// The slider track math: endpoints land on 0 and max, the midpoint
    /// rounds to the nearest step, and off-track fractions clamp.
    #[test]
    fn value_at_fraction_rounds_and_clamps() {
        assert_eq!(value_at_fraction(0.0, 0x1F), 0);
        assert_eq!(value_at_fraction(1.0, 0x1F), 0x1F);
        assert_eq!(value_at_fraction(0.5, 0x3E), 0x1F);
        assert_eq!(value_at_fraction(-0.4, 0x1F), 0);
        assert_eq!(value_at_fraction(1.7, 0x3F), 0x3F);
    }
}
