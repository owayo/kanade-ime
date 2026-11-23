//! Shows a mode hint only for a selected input-source transition.
use super::*;
use objc2::rc::Weak;
use objc2_app_kit::{
    NSBox, NSBoxType, NSColor, NSFloatingWindowLevel, NSFont, NSPopUpMenuWindowLevel, NSTextField,
    NSWindowCollectionBehavior, NSWorkspaceActiveSpaceDidChangeNotification,
    NSWorkspaceDidActivateApplicationNotification,
};
use objc2_foundation::{
    NSDistributedNotificationCenter, NSNotification, NSNotificationSuspensionBehavior, NSRunLoop,
    NSRunLoopCommonModes, NSTimer,
};
use std::ffi::c_void;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentKeyboardInputSource() -> *const c_void;
    fn TISGetInputSourceProperty(source: *const c_void, key: *const c_void) -> *const c_void;
    static kTISPropertyBundleID: *const c_void;
    static kTISPropertyInputSourceType: *const c_void;
    static kTISPropertyInputSourceIsASCIICapable: *const c_void;
    static kTISTypeKeyboardLayout: *const c_void;
    static kTISNotifySelectedKeyboardInputSourceChanged: *const c_void;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFEqual(a: *const c_void, b: *const c_void) -> u8;
    fn CFBooleanGetValue(value: *const c_void) -> u8;
}

const TRANSITION: Duration = Duration::from_millis(400);
const DISPLAY: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceMode {
    Hiragana,
    Ascii,
    Other,
}
impl SourceMode {
    fn label(self) -> Option<&'static str> {
        match self {
            Self::Hiragana => Some("あ  ひらがな"),
            Self::Ascii => Some("A  半角英数"),
            Self::Other => None,
        }
    }
}

#[derive(Default)]
struct Transition {
    current: Option<SourceMode>,
    pending: Option<(SourceMode, Instant)>,
}
impl Transition {
    fn observe(&mut self, mode: SourceMode, now: Instant) -> bool {
        if self.current == Some(mode) {
            return false;
        }
        let previous = self.current.replace(mode);
        self.pending = match mode {
            SourceMode::Hiragana => Some((mode, now)),
            SourceMode::Ascii if previous == Some(SourceMode::Hiragana) => Some((mode, now)),
            _ => None,
        };
        true
    }
    fn pending_mode(&mut self, now: Instant) -> Option<SourceMode> {
        let (mode, since) = self.pending?;
        if now.duration_since(since) > TRANSITION {
            self.pending = None;
            return None;
        }
        Some(mode)
    }
}

#[derive(Clone, Copy)]
pub(super) struct ModeAnchor {
    caret: NSRect,
    window_level: isize,
    app_pid: i32,
}

struct IndicatorState {
    bundle: Retained<NSString>,
    active: RefCell<Option<Weak<Controller>>>,
    transition: RefCell<Transition>,
    entry_anchor: Cell<Option<(ModeAnchor, Instant)>>,
    exit_anchor: Cell<Option<(ModeAnchor, Instant)>>,
    panel: RefCell<Option<Retained<NSPanel>>>,
    timer: RefCell<Option<Retained<NSTimer>>>,
}
define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = IndicatorState]
    #[name = "KanadeInputModeIndicator"]
    struct Indicator;
    impl Indicator {
        #[unsafe(method(kanadeSelectedSourceChanged:))]
        fn selected_source_changed(&self, _: &NSNotification) {
            if MainThreadMarker::new().is_some() {
                guarded(|| self.refresh_source());
            }
        }

        #[unsafe(method(kanadeForegroundChanged:))]
        fn foreground_changed(&self, _: &NSNotification) {
            if MainThreadMarker::new().is_some() {
                guarded(|| {
                    self.dismiss();
                    self.ivars().entry_anchor.set(None);
                    self.ivars().exit_anchor.set(None);
                });
            }
        }

        #[unsafe(method(kanadeModeIndicatorExpired:))]
        fn expired(&self, timer: &NSTimer) {
            guarded(|| {
                let current = self.ivars().timer.borrow().as_deref().is_some_and(|t| {
                    std::ptr::eq(t, timer)
                });
                if current {
                    self.dismiss();
                }
            });
        }
    }
);

thread_local! {
    static MODE_INDICATOR: OnceCell<Retained<Indicator>> = const { OnceCell::new() };
}

pub(super) fn install(bundle: &NSString, mtm: MainThreadMarker) {
    guarded(|| {
        let target = Indicator::alloc(mtm).set_ivars(IndicatorState {
            bundle: bundle.retain(),
            active: RefCell::new(None),
            transition: RefCell::new(Transition::default()),
            entry_anchor: Cell::new(None),
            exit_anchor: Cell::new(None),
            panel: RefCell::new(None),
            timer: RefCell::new(None),
        });
        // SAFETY: NSObject initializes our observer on the main thread.
        let target: Retained<Indicator> = unsafe { msg_send![super(target), init] };
        MODE_INDICATOR.with(|slot| {
            if slot.set(target.clone()).is_err() {
                return;
            }
            // SAFETY: TIS defines the name as a CFString, toll-free bridged
            // to NSString. The observer lives on the main thread.
            unsafe {
                let name = &*kTISNotifySelectedKeyboardInputSourceChanged.cast::<NSString>();
                NSDistributedNotificationCenter::defaultCenter()
                    .addObserver_selector_name_object_suspensionBehavior(
                        &target,
                        sel!(kanadeSelectedSourceChanged:),
                        Some(name),
                        None,
                        NSNotificationSuspensionBehavior::DeliverImmediately,
                    );
                let workspace = NSWorkspace::sharedWorkspace().notificationCenter();
                for name in [
                    NSWorkspaceDidActivateApplicationNotification,
                    NSWorkspaceActiveSpaceDidChangeNotification,
                ] {
                    workspace.addObserver_selector_name_object(
                        &target,
                        sel!(kanadeForegroundChanged:),
                        Some(name),
                        None,
                    );
                }
            }
            target.refresh_source();
        });
    });
}

fn indicator(action: impl FnOnce(&Indicator)) {
    guarded(|| {
        MODE_INDICATOR.with(|slot| {
            if let Some(indicator) = slot.get() {
                action(indicator);
            }
        });
    });
}

pub(super) fn activated(controller: &Controller, sender: Option<&AnyObject>) {
    indicator(|hint| {
        if !hint.active_is(controller) {
            hint.dismiss();
            hint.ivars().exit_anchor.set(None);
        }
        *hint.ivars().active.borrow_mut() = Some(Weak::new(controller));
        let needs_entry_anchor = {
            let mut state = hint.ivars().transition.borrow_mut();
            state.current != Some(SourceMode::Hiragana)
                || state.pending_mode(Instant::now()) == Some(SourceMode::Hiragana)
        };
        if needs_entry_anchor {
            let anchor = sender.and_then(capture_anchor);
            hint.ivars()
                .entry_anchor
                .set(anchor.map(|anchor| (anchor, Instant::now())));
        }
        hint.refresh_source();
    });
}

pub(super) fn deactivated(controller: &Controller, sender: Option<&AnyObject>) {
    indicator(|hint| {
        if !hint.active_is(controller) {
            return;
        }
        hint.ivars().active.borrow_mut().take();
        hint.ivars().entry_anchor.set(None);
        hint.dismiss();
        let anchor = sender.and_then(capture_anchor);
        hint.ivars()
            .exit_anchor
            .set(anchor.map(|anchor| (anchor, Instant::now())));
        hint.refresh_source();
    });
}

pub(super) fn switched_to_ascii(controller: &Controller, anchor: Option<ModeAnchor>) {
    indicator(|hint| {
        if hint.active_is(controller) {
            hint.ivars().active.borrow_mut().take();
        }
        hint.ivars().entry_anchor.set(None);
        hint.ivars()
            .exit_anchor
            .set(anchor.map(|anchor| (anchor, Instant::now())));
        hint.refresh_source();
    });
}

impl Indicator {
    fn active_is(&self, controller: &Controller) -> bool {
        self.ivars()
            .active
            .borrow()
            .as_ref()
            .and_then(Weak::load)
            .is_some_and(|active| std::ptr::eq(&*active, controller))
    }

    fn refresh_source(&self) {
        if unsafe { IsSecureEventInputEnabled() } {
            self.dismiss();
            self.ivars().entry_anchor.set(None);
            self.ivars().exit_anchor.set(None);
            self.ivars().transition.borrow_mut().pending = None;
            return;
        }
        let Some(mode) = current_source_mode(&self.ivars().bundle) else {
            return;
        };
        let now = Instant::now();
        let changed = self.ivars().transition.borrow_mut().observe(mode, now);
        if changed {
            self.dismiss();
            if mode != SourceMode::Ascii {
                self.ivars().exit_anchor.set(None);
            }
        }
        let pending = self.ivars().transition.borrow_mut().pending_mode(now);
        // Notifications can arrive on either side of the IMK lifecycle
        // callbacks. Only geometry is used here, never a client proxy.
        let anchor = match pending {
            Some(SourceMode::Hiragana) => self.ivars().entry_anchor.take(),
            Some(SourceMode::Ascii) => self.ivars().exit_anchor.take(),
            _ => None,
        }
        .filter(|(_, at)| now.duration_since(*at) <= TRANSITION)
        .map(|(anchor, _)| anchor);
        if let (Some(mode), Some(anchor)) = (pending, anchor) {
            let same_transition = {
                let mut state = self.ivars().transition.borrow_mut();
                if state.pending_mode(Instant::now()) == Some(mode) {
                    state.pending.take();
                    true
                } else {
                    false
                }
            };
            if same_transition {
                self.present(mode, anchor);
            }
        }
    }

    fn dismiss(&self) {
        if let Some(timer) = self.ivars().timer.borrow_mut().take() {
            timer.invalidate();
        }
        if let Some(panel) = self.ivars().panel.borrow().as_ref() {
            panel.orderOut(None);
        }
    }

    fn present(&self, mode: SourceMode, anchor: ModeAnchor) {
        if unsafe { IsSecureEventInputEnabled() }
            || NSWorkspace::sharedWorkspace()
                .frontmostApplication()
                .is_none_or(|app| app.processIdentifier() != anchor.app_pid)
        {
            return;
        }
        let Some(title) = mode.label() else { return };
        let mtm = self.mtm();
        let label = NSTextField::labelWithString(&NSString::from_str(title), mtm);
        label.setFont(Some(&NSFont::systemFontOfSize(13.0)));
        let fit = label.fittingSize();
        let size = NSSize::new(fit.width.ceil() + 22.0, fit.height.ceil() + 12.0);
        let screen = NSScreen::screens(mtm).into_iter().find(|screen| {
            let frame = screen.frame();
            let point = NSPoint::new(
                anchor.caret.origin.x,
                anchor.caret.origin.y + anchor.caret.size.height / 2.0,
            );
            point.x >= frame.origin.x
                && point.x < frame.origin.x + frame.size.width
                && point.y >= frame.origin.y
                && point.y < frame.origin.y + frame.size.height
        });
        let Some(frame) =
            screen.and_then(|screen| placement(anchor.caret, size, screen.visibleFrame()))
        else {
            return;
        };
        self.dismiss();
        let panel = self
            .ivars()
            .panel
            .borrow_mut()
            .get_or_insert_with(|| {
                let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
                    NSPanel::alloc(mtm),
                    frame,
                    NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                    NSBackingStoreType::Buffered,
                    false,
                );
                panel.setFloatingPanel(true);
                panel.setBecomesKeyOnlyIfNeeded(true);
                panel.setIgnoresMouseEvents(true);
                panel.setHidesOnDeactivate(false);
                panel.setBackgroundColor(Some(&NSColor::clearColor()));
                panel.setOpaque(false);
                panel.setHasShadow(true);
                panel.setCollectionBehavior(
                    NSWindowCollectionBehavior::CanJoinAllSpaces
                        | NSWindowCollectionBehavior::FullScreenAuxiliary
                        | NSWindowCollectionBehavior::Transient
                        | NSWindowCollectionBehavior::IgnoresCycle,
                );
                panel
            })
            .clone();
        let background = NSBox::initWithFrame(NSBox::alloc(mtm), NSRect::new(NSPoint::ZERO, size));
        background.setBoxType(NSBoxType::Custom);
        background.setCornerRadius(6.0);
        background.setBorderWidth(1.0);
        background.setBorderColor(&NSColor::separatorColor());
        background.setFillColor(&NSColor::windowBackgroundColor());
        label.setFrame(NSRect::new(NSPoint::new(11.0, 6.0), fit));
        background.addSubview(&label);
        panel.setContentView(Some(&background));
        panel.setTitle(&label.stringValue());
        panel.setLevel(
            anchor
                .window_level
                .saturating_add(1)
                .clamp(NSFloatingWindowLevel, NSPopUpMenuWindowLevel - 1),
        );
        panel.setFrame_display(frame, true);
        let timer = unsafe {
            NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
                DISPLAY.as_secs_f64(),
                self,
                sel!(kanadeModeIndicatorExpired:),
                None,
                false,
            )
        };
        unsafe {
            NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
        }
        *self.ivars().timer.borrow_mut() = Some(timer);
        panel.orderFrontRegardless();
    }
}

fn current_source_mode(bundle: &NSString) -> Option<SourceMode> {
    // SAFETY: TIS' Copy function returns an owned reference. The properties
    // remain valid through CFRelease; no private input data is queried.
    unsafe {
        let source = TISCopyCurrentKeyboardInputSource();
        if source.is_null() {
            return None;
        }
        let owner = TISGetInputSourceProperty(source, kTISPropertyBundleID);
        let kind = TISGetInputSourceProperty(source, kTISPropertyInputSourceType);
        let ascii = TISGetInputSourceProperty(source, kTISPropertyInputSourceIsASCIICapable);
        let mode = if !owner.is_null() && &*owner.cast::<NSString>() == bundle {
            SourceMode::Hiragana
        } else if !kind.is_null()
            && CFEqual(kind, kTISTypeKeyboardLayout) != 0
            && !ascii.is_null()
            && CFBooleanGetValue(ascii) != 0
        {
            SourceMode::Ascii
        } else {
            SourceMode::Other
        };
        CFRelease(source);
        Some(mode)
    }
}

pub(super) fn capture_anchor(sender: &AnyObject) -> Option<ModeAnchor> {
    if unsafe { IsSecureEventInputEnabled() } {
        return None;
    }
    let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
    let front = app.bundleIdentifier()?;
    let id: Option<Retained<NSString>> = guarded(|| unsafe { msg_send![sender, bundleIdentifier] });
    if id.as_deref() != Some(&*front) {
        return None;
    }
    let caret = guarded(|| unsafe {
        let selected: NSRange = msg_send![sender, selectedRange];
        let end = selected.location.checked_add(selected.length)?;
        if end >= NSNotFound as usize {
            return None;
        }
        let rect: NSRect = msg_send![sender, firstRectForCharacterRange:NSRange::new(end, 0), actualRange:std::ptr::null_mut::<NSRange>()];
        valid_caret(rect).then_some(rect)
    }).or_else(|| guarded(|| unsafe {
        // IMK clients that do not expose NSTextInputClient give the current
        // insertion line through the same query used by the candidate panel.
        let mut rect = NSRect::ZERO;
        let _: Option<Retained<AnyObject>> =
            msg_send![sender, attributesForCharacterIndex:0usize, lineHeightRectangle:&mut rect];
        valid_caret(rect).then_some(rect)
    }))?;
    let window_level = guarded(|| unsafe { msg_send![sender, windowLevel] });
    Some(ModeAnchor {
        caret,
        window_level,
        app_pid: app.processIdentifier(),
    })
}

fn valid_caret(rect: NSRect) -> bool {
    rect.origin.x.is_finite()
        && rect.origin.y.is_finite()
        && rect.size.width.is_finite()
        && rect.size.width >= 0.0
        && rect.size.height.is_finite()
        && rect.size.height > 0.0
}

fn placement(caret: NSRect, size: NSSize, visible: NSRect) -> Option<NSRect> {
    const GAP: f64 = 6.0;
    if visible.size.width < size.width || visible.size.height < size.height {
        return None;
    }
    let min_y = visible.origin.y;
    let max_y = min_y + visible.size.height - size.height;
    let above = caret.origin.y + caret.size.height + GAP;
    let below = caret.origin.y - GAP - size.height;
    // Prefer the space above the selection because AppKit can display its
    // own insertion-point accessory below.
    let y = if above <= max_y { above } else { below };
    let origin = NSPoint::new(
        caret.origin.x.clamp(
            visible.origin.x,
            visible.origin.x + visible.size.width - size.width,
        ),
        y.clamp(min_y, max_y),
    );
    Some(NSRect::new(origin, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_transitions_involving_the_ime_offer_a_label() {
        let mut tracker = Transition::default();
        let now = Instant::now();
        assert!(tracker.observe(SourceMode::Ascii, now));
        assert_eq!(tracker.pending_mode(now), None);
        assert!(tracker.observe(SourceMode::Hiragana, now));
        assert_eq!(tracker.pending_mode(now), Some(SourceMode::Hiragana));
        tracker.pending.take();
        assert!(!tracker.observe(SourceMode::Hiragana, now));
        assert_eq!(tracker.pending_mode(now), None);
        assert!(tracker.observe(SourceMode::Ascii, now));
        assert_eq!(tracker.pending_mode(now), Some(SourceMode::Ascii));
        assert_eq!(
            tracker.pending_mode(now + TRANSITION + Duration::from_millis(1)),
            None
        );
        tracker.observe(SourceMode::Other, now);
        assert_eq!(tracker.pending_mode(now), None);
    }

    #[test]
    fn indicator_uses_the_carets_display_and_flips_near_its_edge() {
        let display = NSRect::new(NSPoint::new(-1000.0, 20.0), NSSize::new(1000.0, 600.0));
        let size = NSSize::new(94.0, 30.0);
        let normal = NSRect::new(NSPoint::new(-500.0, 70.0), NSSize::new(0.0, 20.0));
        let edge = NSRect::new(NSPoint::new(-3.0, 585.0), NSSize::new(0.0, 18.0));
        assert_eq!(
            placement(normal, size, display).unwrap().origin,
            NSPoint::new(-500.0, 96.0)
        );
        assert_eq!(
            placement(edge, size, display).unwrap().origin,
            NSPoint::new(-94.0, 549.0)
        );
        assert!(!valid_caret(NSRect::ZERO));
    }
}
