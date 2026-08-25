// GGO: a continuous control for the fork's panels (zoom, palette
// sub-index, ghost opacity), which had nothing but `-`/`+` steppers.

use gpui::{
    App, Context, DragMoveEvent, ElementId, IntoElement, MouseButton, MouseDownEvent, Pixels,
    Render, Window, px,
};

use crate::prelude::*;

/// A horizontal track with a thumb. `value` is normalised to `0.0..=1.0`;
/// the caller maps its own range and snaps on change. Mouse-down and
/// drag along the track set the value from the pointer's x position.
/// No keyboard handling of its own: panels keep their key and wheel
/// steps.
#[derive(IntoElement)]
pub struct Slider {
    id: ElementId,
    value: f32,
    width: Pixels,
    disabled: bool,
    on_change: Option<Box<dyn Fn(f32, &mut Window, &mut App) + 'static>>,
}

/// The drag payload: the slider's id, so the drag-move listener can tell
/// its own drag from another element's.
#[derive(Clone)]
struct SliderDrag(ElementId);

impl Render for SliderDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

impl Slider {
    pub fn new(id: impl Into<ElementId>, value: f32) -> Self {
        Self {
            id: id.into(),
            value: value.clamp(0.0, 1.0),
            width: px(96.),
            disabled: false,
            on_change: None,
        }
    }

    pub fn width(mut self, width: Pixels) -> Self {
        self.width = width;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn on_change(mut self, f: impl Fn(f32, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Box::new(f));
        self
    }
}

const TRACK_HEIGHT: Pixels = px(4.);
const THUMB: Pixels = px(10.);
const HIT_HEIGHT: Pixels = px(16.);

/// The normalised value for a pointer `x` over a track of `width`
/// starting at `origin_x`. The pointer maps onto the THUMB's travel
/// (`width - THUMB`, measured from the thumb's centre), so grabbing the
/// thumb does not jump it and the ends are reachable.
pub fn slider_value_at(x: Pixels, origin_x: Pixels, width: Pixels) -> f32 {
    let travel = f32::from(width - THUMB);
    if travel <= 0.0 {
        return 0.0;
    }
    ((f32::from(x - origin_x) - f32::from(THUMB) / 2.0) / travel).clamp(0.0, 1.0)
}

impl RenderOnce for Slider {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = cx.theme().colors();
        let on_change = self.on_change.map(std::rc::Rc::new);
        let width = self.width;
        let disabled = self.disabled;
        let id = self.id.clone();
        let fill = colors.text_accent;
        let thumb_left = px(f32::from(width - THUMB) * self.value);
        let bounds = std::rc::Rc::new(std::cell::RefCell::new(None::<gpui::Bounds<Pixels>>));

        let track = div()
            .absolute()
            .left_0()
            .top(px(f32::from(HIT_HEIGHT - TRACK_HEIGHT) / 2.0))
            .w(width)
            .h(TRACK_HEIGHT)
            .rounded_full()
            .bg(colors.element_background)
            .child(
                div()
                    .h_full()
                    .w(thumb_left + THUMB / 2.0)
                    .rounded_full()
                    .bg(if disabled {
                        colors.element_disabled
                    } else {
                        fill
                    }),
            );
        let thumb = div()
            .absolute()
            .left(thumb_left)
            .top(px(f32::from(HIT_HEIGHT - THUMB) / 2.0))
            .size(THUMB)
            .rounded_full()
            .bg(if disabled {
                colors.element_disabled
            } else {
                fill
            })
            .border_1()
            .border_color(colors.border);

        let record_bounds = {
            let bounds = bounds.clone();
            gpui::canvas(
                move |b, _window, _cx| {
                    *bounds.borrow_mut() = Some(b);
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full()
        };

        div()
            .id(id.clone())
            .relative()
            .w(width)
            .h(HIT_HEIGHT)
            .cursor_pointer()
            .child(record_bounds)
            .child(track)
            .child(thumb)
            .when(!disabled, |el| {
                let bounds_down = bounds.clone();
                let on_change_down = on_change.clone();
                let bounds_move = bounds.clone();
                let on_change_move = on_change.clone();
                let drag_id = id.clone();
                let drag_id_move = id.clone();
                el.on_mouse_down(
                    MouseButton::Left,
                    move |event: &MouseDownEvent, window, cx| {
                        if let (Some(b), Some(on_change)) = (*bounds_down.borrow(), &on_change_down)
                        {
                            on_change(
                                slider_value_at(event.position.x, b.origin.x, b.size.width),
                                window,
                                cx,
                            );
                        }
                    },
                )
                .on_drag(SliderDrag(drag_id), |drag, _offset, _window, cx| {
                    cx.new(|_| drag.clone())
                })
                .on_drag_move(
                    move |event: &DragMoveEvent<SliderDrag>, window, cx| {
                        if event.drag(cx).0 != drag_id_move {
                            return;
                        }
                        if let (Some(b), Some(on_change)) = (*bounds_move.borrow(), &on_change_move)
                        {
                            on_change(
                                slider_value_at(event.event.position.x, b.origin.x, b.size.width),
                                window,
                                cx,
                            );
                        }
                    },
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slider_value_maps_pointer_x_onto_the_track_and_clamps() {
        // The thumb's centre travels from THUMB/2 to width - THUMB/2.
        assert_eq!(slider_value_at(px(5.), px(0.), px(100.)), 0.0);
        assert_eq!(slider_value_at(px(50.), px(0.), px(100.)), 0.5);
        assert_eq!(slider_value_at(px(95.), px(0.), px(100.)), 1.0);
        assert_eq!(
            slider_value_at(px(150.), px(100.), px(100.)),
            0.5,
            "origin offset"
        );
        assert_eq!(
            slider_value_at(px(-10.), px(0.), px(100.)),
            0.0,
            "clamped low"
        );
        assert_eq!(
            slider_value_at(px(500.), px(0.), px(100.)),
            1.0,
            "clamped high"
        );
        assert_eq!(
            slider_value_at(px(5.), px(0.), px(0.)),
            0.0,
            "a track thinner than the thumb is safe"
        );
    }
}
