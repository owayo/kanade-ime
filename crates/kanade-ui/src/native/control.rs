//! Focus and activation shared by settings buttons, switches and choices.
use gpui::{prelude::*, *};
use std::{cell::Cell, rc::Rc};

struct FocusState {
    handle: FocusHandle,
    focused: bool,
}

#[derive(IntoElement)]
pub(super) struct Control {
    id: ElementId,
    inner: Stateful<Div>,
    disabled: Rc<Cell<bool>>,
}
impl Control {
    pub(super) fn new(id: impl Into<ElementId>, inner: Stateful<Div>) -> Self {
        Self {
            id: id.into(),
            inner,
            disabled: Rc::new(Cell::new(false)),
        }
    }
    pub(super) fn inactive(self, inactive: bool) -> Self {
        self.disabled.set(self.disabled.get() || inactive);
        self
    }
    pub(super) fn on_click(
        mut self,
        listener: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        let disabled = self.disabled.clone();
        self.inner = self.inner.on_click(move |event, window, cx| {
            if !disabled.get() {
                listener(event, window, cx);
            }
        });
        self
    }
}
impl Styled for Control {
    fn style(&mut self) -> &mut StyleRefinement {
        self.inner.style()
    }
}
impl InteractiveElement for Control {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.inner.interactivity()
    }
}
impl StatefulInteractiveElement for Control {}
impl ParentElement for Control {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.inner.extend(elements);
    }
}

actions!(kanade_controls, [NextControl, PreviousControl]);

/// Global fallback also works before any element has focus. Text fields keep
/// their more specific marked-text key map.
pub(super) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", NextControl, None),
        KeyBinding::new("shift-tab", PreviousControl, None),
    ]);
    cx.on_action(|_: &NextControl, cx| navigate(true, cx));
    cx.on_action(|_: &PreviousControl, cx| navigate(false, cx));
}

fn navigate(forward: bool, cx: &mut App) {
    // Actions run inside a window update; do not enter it recursively.
    let window = cx.active_window().or_else(|| cx.windows().first().copied());
    cx.defer(move |cx| {
        if let Some(window) = window {
            let _ = window.update(cx, |_, window, cx| {
                if forward {
                    window.focus_next(cx);
                } else {
                    window.focus_prev(cx);
                }
            });
        }
    });
}

/// Scope autoscroll requests to descendants of one scroll body. Fixed headers
/// and confirmations must not move that body's contents.
pub(super) fn scroll_begin() -> impl IntoElement {
    canvas(
        |_, window, _| {
            window.take_autoscroll();
        },
        |_, _, _, _| {},
    )
    .absolute()
    .w(px(0.))
    .h(px(0.))
}

/// Div does not implement List's autoscroll protocol in this GPUI version.
/// Apply the smallest bounded offset after prepaint, then draw another frame.
/// This avoids prepainting FnOnce canvases a second time in the same frame.
pub(super) fn scroll_end(handle: ScrollHandle) -> impl IntoElement {
    canvas(
        move |_, window, cx| {
            if let Some(target) = window.take_autoscroll() {
                window.defer(cx, move |window, _| {
                    let viewport = handle.bounds();
                    let offset = handle.offset();
                    let delta = if target.top() < viewport.top()
                        || target.size.height > viewport.size.height
                    {
                        viewport.top() - target.top()
                    } else if target.bottom() > viewport.bottom() {
                        viewport.bottom() - target.bottom()
                    } else {
                        px(0.)
                    };
                    let next = (offset.y + delta).max(-handle.max_offset().y).min(px(0.));
                    if next != offset.y {
                        handle.set_offset(point(offset.x, next));
                        window.refresh();
                    }
                });
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .w(px(0.))
    .h(px(0.))
}

impl RenderOnce for Control {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(self.id, cx, |_, cx| FocusState {
            handle: cx.focus_handle().tab_index(0).tab_stop(true),
            focused: false,
        });
        let focus = state.read(cx).handle.clone().tab_stop(!self.disabled.get());
        let scroll_focus = focus.clone();
        let mouse_focus = focus.clone();
        let disabled = self.disabled.clone();
        let mut inner = self
            .inner
            .relative()
            .track_focus(&focus)
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                if !disabled.get() {
                    window.focus(&mouse_focus, cx);
                }
            })
            .focus(|style| style.bg(rgb(0xe1eaff)));
        if self.disabled.get() {
            inner = inner
                .aria_description("現在は操作できません。")
                .text_color(rgb(0x657080))
                .cursor_default();
        }
        inner.child(
            canvas(
                move |bounds, window, cx| {
                    let focused = scroll_focus.is_focused(window);
                    let previous = state.update(cx, |state, _| {
                        let old = state.focused;
                        state.focused = focused;
                        old
                    });
                    if focused && !previous {
                        window.request_autoscroll(bounds);
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        )
    }
}
