//! AppKit presentation of the GPUI settings window.
//!
//! GPUI centres a new window on the primary display, and reports a reopen
//! request only while no window is visible. An explicit request from the input
//! menu must show the window where the user is working, so this layer selects
//! the display under the pointer, restores a minimized window, lets the window
//! follow the active Space and orders it front at its normal level.
use crate::placement::{self, Rect, Screen, WindowState};
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, class, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObject, Sel},
    sel,
};
use objc2_app_kit::{
    NSEvent, NSScreen, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowOcclusionState,
    NSWindowStyleMask,
};
use objc2_foundation::{NSNumber, NSPoint, NSRect, ns_string};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::panic::{AssertUnwindSafe, catch_unwind};

/// `kCoreEventClass` and `kAEReopenApplication`.
const CORE_EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt");
const REOPEN_APPLICATION: u32 = u32::from_be_bytes(*b"rapp");

/// Reads metadata from the explicitly selected application without loading code.
pub(super) fn application_id(path: &std::path::Path) -> Option<String> {
    if !path.is_dir() || path.extension()? != "app" {
        return None;
    }
    let path = objc2_foundation::NSString::from_str(path.to_str()?);
    let bundle = objc2_foundation::NSBundle::bundleWithPath(&path)?;
    let id = bundle.bundleIdentifier()?.to_string();
    kanade_indexer::management::valid_apps(std::slice::from_ref(&id)).then_some(id)
}

fn rect(value: NSRect) -> Rect {
    Rect::new(
        value.origin.x,
        value.origin.y,
        value.size.width,
        value.size.height,
    )
}

/// Attached displays, their geometry and the index of the one under the pointer.
fn displays(mtm: MainThreadMarker) -> (Vec<Retained<NSScreen>>, Vec<Screen>, Option<usize>) {
    let screens = NSScreen::screens(mtm).to_vec();
    let frames: Vec<Screen> = screens
        .iter()
        .map(|screen| Screen {
            frame: rect(screen.frame()),
            visible: rect(screen.visibleFrame()),
        })
        .collect();
    let pointer = NSEvent::mouseLocation();
    let target = placement::screen_at(&frames, (pointer.x, pointer.y));
    (screens, frames, target)
}

/// CoreGraphics identifier of the display under the pointer. The request comes
/// from a menu the user has just clicked, so this is the display in use.
pub(super) fn pointer_display() -> Option<u32> {
    let (screens, _, target) = displays(MainThreadMarker::new()?);
    let number = screens
        .get(target?)?
        .deviceDescription()
        .objectForKey(ns_string!("NSScreenNumber"))?;
    Some(number.downcast::<NSNumber>().ok()?.unsignedIntValue())
}

pub(super) fn native_window(window: &gpui::Window) -> Option<Retained<NSWindow>> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return None;
    };
    // SAFETY: GPUI reports the live content view of a window that is borrowed
    // on the main thread for the duration of this call.
    let view = unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<NSView>()) }?;
    view.window()
}

pub(super) struct Presented {
    pub restored: bool,
    pub relocated: bool,
}

/// Shows the window for an explicit open request.
///
/// Call this outside a GPUI update. AppKit notifies GPUI's window delegate
/// synchronously while the frame and key status change.
pub(super) fn present(window: &NSWindow) -> Presented {
    let fullscreen = window.styleMask().contains(NSWindowStyleMask::FullScreen);
    if !fullscreen {
        // Activation brings the window to the Space in use instead of leaving
        // it on the Space where it was opened. The window level is unchanged.
        let mut behavior = window.collectionBehavior();
        behavior.remove(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenPrimary
                | NSWindowCollectionBehavior::FullScreenNone,
        );
        behavior.insert(
            NSWindowCollectionBehavior::MoveToActiveSpace
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        window.setCollectionBehavior(behavior);
    }
    let (_, frames, target) = displays(window.mtm());
    let plan = placement::plan(
        WindowState {
            frame: rect(window.frame()),
            miniaturized: window.isMiniaturized(),
            fullscreen,
        },
        &frames,
        target,
    );
    if let Some((x, y)) = plan.origin {
        window.setFrameOrigin(NSPoint::new(x, y));
    }
    if plan.restore {
        window.deminiaturize(None);
    }
    window.makeKeyAndOrderFront(None);
    // The request arrives from another process, so this application may not be
    // active yet. Ordering within the normal level does not depend on that.
    window.orderFrontRegardless();
    Presented {
        restored: plan.restore,
        relocated: plan.origin.is_some(),
    }
}

pub(super) enum Visibility {
    Visible,
    Occluded,
    OffSpace,
}

pub(super) fn visibility(window: &NSWindow) -> Visibility {
    if !window.isOnActiveSpace() {
        Visibility::OffSpace
    } else if window
        .occlusionState()
        .contains(NSWindowOcclusionState::Visible)
    {
        Visibility::Visible
    } else {
        Visibility::Occluded
    }
}

fn reopen_selector() -> Sel {
    sel!(handleReopen:withReplyEvent:)
}
struct ReopenIvars {
    callback: Box<dyn Fn()>,
}
define_class!(
    // SAFETY: `NSObject` has no subclassing requirements, and the handler is
    // created and called on the main thread only.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ReopenIvars]
    #[name = "KanadeSettingsReopenHandler"]
    struct ReopenHandler;
    impl ReopenHandler {
        #[unsafe(method(handleReopen:withReplyEvent:))]
        fn handle_reopen(&self, _event: Option<&AnyObject>, _reply: Option<&AnyObject>) {
            let _ = catch_unwind(AssertUnwindSafe(|| (self.ivars().callback)()));
        }
    }
);

/// Calls `callback` for every reopen request sent to the running application.
///
/// AppKit's default handler forwards the request to the application delegate,
/// and GPUI's delegate drops it while a window is visible, including a window
/// on another Space or display. Registering for the Apple event replaces that
/// default, so the callback must restore and show the window itself.
pub(super) fn on_reopen(callback: impl Fn() + 'static) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let handler = mtm.alloc::<ReopenHandler>().set_ivars(ReopenIvars {
        callback: Box::new(callback),
    });
    // SAFETY: `init` is the designated initializer of `NSObject`.
    let handler: Retained<ReopenHandler> = unsafe { msg_send![super(handler), init] };
    // SAFETY: The selector takes the event and its reply, as the event manager
    // requires, and both event codes are four-character `OSType` values.
    unsafe {
        let manager: Option<Retained<AnyObject>> =
            msg_send![class!(NSAppleEventManager), sharedAppleEventManager];
        let Some(manager) = manager else {
            return;
        };
        let _: () = msg_send![
            &*manager,
            setEventHandler: &*handler,
            andSelector: reopen_selector(),
            forEventClass: CORE_EVENT_CLASS,
            andEventID: REOPEN_APPLICATION,
        ];
    }
    // The event manager does not own its handlers. This one serves the whole
    // process lifetime.
    std::mem::forget(handler);
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2::ClassType;

    #[test]
    fn reopen_handler_implements_the_selector_given_to_the_event_manager() {
        // The event manager calls the handler by selector with the event and
        // its reply; a mismatch would silently drop every reopen request.
        let method = ReopenHandler::class()
            .instance_method(reopen_selector())
            .expect("handler method is registered");
        assert_eq!(
            method.name().name().to_str(),
            Ok("handleReopen:withReplyEvent:")
        );
        // Receiver, selector, event and reply.
        assert_eq!(method.arguments_count(), 4);
    }

    #[test]
    fn reopen_event_codes_match_the_core_suite() {
        assert_eq!(CORE_EVENT_CLASS, 0x6165_7674);
        assert_eq!(REOPEN_APPLICATION, 0x7261_7070);
    }
}
