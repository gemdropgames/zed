// GGO: a continuous control for the fork's panels (zoom, palette
// sub-index, ghost opacity), which had nothing but `-`/`+` steppers.

use gpui::{
    AnyWindowHandle, App, Context, DragMoveEvent, ElementId, IntoElement, MouseButton,
    MouseDownEvent, Pixels, Render, Window, px,
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
    on_change: Option<Box<dyn Fn(f32, &mut Window, &mut App) + 'static>>,
}

/// The drag payload. The active drag is app-global, so it carries the
/// WINDOW as well as the element id: two windows showing the same editor
/// have sliders with identical ids, and without the window a drag in one
/// would drive the other.
#[derive(Clone, PartialEq)]
struct SliderDrag(ElementId, AnyWindowHandle);

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
            on_change: None,
        }
    }

    pub fn width(mut self, width: Pixels) -> Self {
        self.width = width;
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

impl Slider {
    /// The normalised value for a pointer `x` over a track of `width`
    /// starting at `origin_x`. The pointer maps onto the THUMB's travel
    /// (`width - THUMB`, measured from the thumb's centre), so grabbing
    /// the thumb does not jump it and the ends are reachable.
    pub fn value_at(x: Pixels, origin_x: Pixels, width: Pixels) -> f32 {
        let travel = f32::from(width - THUMB);
        if travel <= 0.0 {
            return 0.0;
        }
        ((f32::from(x - origin_x) - f32::from(THUMB) / 2.0) / travel).clamp(0.0, 1.0)
    }
}

/// An integer range as a slider fraction, and back -- every GGO zoom /
/// palette-index slider maps its own `min..=max` this way.
pub fn slider_fraction(value: usize, min: usize, max: usize) -> f32 {
    if max <= min {
        return 0.0;
    }
    (value.clamp(min, max) - min) as f32 / (max - min) as f32
}

pub fn slider_step(fraction: f32, min: usize, max: usize) -> usize {
    if max <= min {
        return min;
    }
    min + (fraction.clamp(0.0, 1.0) * (max - min) as f32).round() as usize
}

impl RenderOnce for Slider {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = cx.theme().colors();
        let on_change = self.on_change.map(std::rc::Rc::new);
        let width = self.width;
        let id = self.id.clone();
        let drag = SliderDrag(id.clone(), window.window_handle());
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
                    .bg(fill),
            );
        let thumb = div()
            .absolute()
            .left(thumb_left)
            .top(px(f32::from(HIT_HEIGHT - THUMB) / 2.0))
            .size(THUMB)
            .rounded_full()
            .bg(fill)
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
            .id(id)
            .relative()
            .w(width)
            .h(HIT_HEIGHT)
            .cursor_pointer()
            .child(record_bounds)
            .child(track)
            .child(thumb)
            .when_some(on_change, |el, on_change| {
                let bounds_down = bounds.clone();
                let on_change_down = on_change.clone();
                let bounds_move = bounds.clone();
                let on_change_move = on_change;
                let drag_move = drag.clone();
                el.on_mouse_down(
                    MouseButton::Left,
                    move |event: &MouseDownEvent, window, cx| {
                        if let Some(b) = *bounds_down.borrow() {
                            on_change_down(
                                Slider::value_at(event.position.x, b.origin.x, b.size.width),
                                window,
                                cx,
                            );
                        }
                    },
                )
                .on_drag(drag, |drag, _offset, _window, cx| cx.new(|_| drag.clone()))
                .on_drag_move(
                    move |event: &DragMoveEvent<SliderDrag>, window, cx| {
                        if *event.drag(cx) != drag_move {
                            return;
                        }
                        if let Some(b) = *bounds_move.borrow() {
                            on_change_move(
                                Slider::value_at(event.event.position.x, b.origin.x, b.size.width),
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
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn slider_value_maps_pointer_x_onto_the_track_and_clamps() {
        // The thumb's centre travels from THUMB/2 to width - THUMB/2.
        assert_eq!(Slider::value_at(px(5.), px(0.), px(100.)), 0.0);
        assert_eq!(Slider::value_at(px(50.), px(0.), px(100.)), 0.5);
        assert_eq!(Slider::value_at(px(95.), px(0.), px(100.)), 1.0);
        assert_eq!(
            Slider::value_at(px(150.), px(100.), px(100.)),
            0.5,
            "origin offset"
        );
        assert_eq!(
            Slider::value_at(px(-10.), px(0.), px(100.)),
            0.0,
            "clamped low"
        );
        assert_eq!(
            Slider::value_at(px(500.), px(0.), px(100.)),
            1.0,
            "clamped high"
        );
        assert_eq!(
            Slider::value_at(px(5.), px(0.), px(0.)),
            0.0,
            "a track thinner than the thumb is safe"
        );
    }

    #[test]
    fn the_integer_range_round_trips_through_the_fraction() {
        assert_eq!(slider_fraction(1, 1, 8), 0.0);
        assert_eq!(slider_fraction(8, 1, 8), 1.0);
        assert_eq!(slider_step(0.0, 1, 8), 1);
        assert_eq!(slider_step(1.0, 1, 8), 8);
        for zoom in 1..=8 {
            assert_eq!(slider_step(slider_fraction(zoom, 1, 8), 1, 8), zoom);
        }
        assert_eq!(slider_fraction(99, 1, 8), 1.0, "clamped high");
        assert_eq!(slider_step(2.0, 0, 15), 15, "clamped high");
        assert_eq!(slider_step(-1.0, 0, 15), 0, "clamped low");
        assert_eq!(slider_fraction(4, 4, 4), 0.0, "a degenerate range is safe");
        assert_eq!(slider_step(0.5, 4, 4), 4, "a degenerate range is safe");
    }

    /// The component end to end: a click on the track reaches `on_change`
    /// with the position mapped onto the thumb's travel.
    #[gpui::test]
    async fn a_click_on_the_track_reports_its_position(cx: &mut gpui::TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc;

        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme::init(theme::LoadThemes::JustBase, cx);
        });
        let changes: Rc<RefCell<Vec<f32>>> = Rc::new(RefCell::new(Vec::new()));
        let (_view, cx) = cx.add_window_view({
            let changes = changes.clone();
            |_window, _cx| SliderHarness { changes }
        });
        cx.run_until_parked();

        // The track is 100px wide at the window origin, so the thumb's
        // travel is 5..=95.
        cx.simulate_event(gpui::MouseDownEvent {
            button: MouseButton::Left,
            position: gpui::point(px(27.5), px(8.)),
            modifiers: Default::default(),
            click_count: 1,
            first_mouse: false,
        });
        cx.run_until_parked();
        let seen = changes.borrow().clone();
        assert_eq!(seen.len(), 1, "one change per click: {seen:?}");
        assert!((seen[0] - 0.25).abs() < 0.01, "clicked at 25%: {seen:?}");
    }

    struct SliderHarness {
        changes: Rc<RefCell<Vec<f32>>>,
    }

    impl Render for SliderHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let changes = self.changes.clone();
            div().size_full().child(
                Slider::new("harness", 0.0)
                    .width(px(100.))
                    .on_change(move |value, _window, _cx| changes.borrow_mut().push(value)),
            )
        }
    }
}
