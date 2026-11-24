mod candidates;
mod refinement;

use anyhow::{Context as _, Result, ensure};
use kanade_ipc::settings_log::{
    Component as LogComponent, Event as LogEvent, record as settings_log,
};
use kanade_ipc::{
    self as ipc,
    diagnostics::{Event as Incident, record as incident},
    frontend::{Client, Document},
    unix::DeadlineStream,
    *,
};
use objc2::{
    AnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class,
    msg_send,
    rc::{Allocated, Retained, autoreleasepool},
    runtime::{AnyObject, NSObjectProtocol, Sel},
    sel,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSButton,
    NSControlStateValueOff, NSControlStateValueOn, NSEvent, NSEventMask, NSEventModifierFlags,
    NSEventType, NSImage, NSImageNameStatusAvailable, NSMenu, NSMenuItem, NSPanel, NSScreen,
    NSWindowStyleMask, NSWorkspace, NSWorkspaceOpenConfiguration,
};
use objc2_foundation::{
    NSAttributedString, NSBundle, NSNotFound, NSObject, NSPoint, NSRange, NSRect, NSSize, NSString,
};
use objc2_input_method_kit::{IMKInputController, IMKServer};
use std::{
    cell::{Cell, OnceCell, RefCell},
    panic::{AssertUnwindSafe, catch_unwind},
    time::{Duration, Instant},
};

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> bool;
    fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> *const std::ffi::c_void;
    fn TISSelectInputSource(source: *const std::ffi::c_void) -> i32;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: *const std::ffi::c_void);
}
type EngineClient = Client<DeadlineStream>;
struct SettingsMenu {
    menu: Retained<NSMenu>,
    status_items: [Retained<NSMenuItem>; 2],
}
fn retained_settings_menu(
    cache: &OnceCell<SettingsMenu>,
    mtm: MainThreadMarker,
    target: &AnyObject,
    status: RuntimeStatus,
) -> Retained<NSMenu> {
    let cached = cache.get_or_init(|| {
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Kanade"));
        menu.setAutoenablesItems(false);
        menu.setShowsStateColumn(true);
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Kanade の設定…"),
                Some(sel!(kanadeOpenSettings:)),
                &NSString::from_str(""),
            )
        };
        // IMK routes custom actions through the document controller. Keep
        // its menu/item identity stable while the system tracks that menu.
        unsafe {
            item.setTarget(Some(target));
        }
        item.setTag(1001);
        item.setEnabled(true);
        menu.addItem(&item);
        // Use the state column so the dot is independent of menu icon visibility.
        let dot = NSImage::imageNamed(unsafe { NSImageNameStatusAvailable });
        let status_items = std::array::from_fn(|_| {
            let item = NSMenuItem::new(mtm);
            if let Some(dot) = &dot {
                unsafe {
                    item.setOnStateImage(Some(dot));
                }
            }
            item.setEnabled(false);
            menu.addItem(&item);
            item
        });
        SettingsMenu { menu, status_items }
    });
    for (item, (label, running)) in cached.status_items.iter().zip(status.menu_rows()) {
        item.setTitle(&NSString::from_str(label));
        item.setState(if running {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
    }
    cached.menu.clone()
}
#[derive(Default)]
struct State {
    refinement: refinement::Polling,
    settings_menu: OnceCell<SettingsMenu>,
    selecting_source: Cell<bool>,
    /// No connection attempt before this time, after one failed.
    connect_after: Cell<Option<Instant>>,
    preedit: RefCell<Preedit>,
    engine: RefCell<Option<EngineClient>>,
    panel: RefCell<Option<Retained<NSPanel>>>,
    sender: RefCell<Option<Retained<AnyObject>>>,
    page: RefCell<Option<CandidatePage>>,
    caret: RefCell<NSRect>,
}
define_class!(
    #[unsafe(super(IMKInputController))]
    #[thread_kind = MainThreadOnly]
    #[ivars = State]
    #[name = "KanadeInputController"]
    struct Controller;
    unsafe impl NSObjectProtocol for Controller {}
    impl Controller {
        #[unsafe(method_id(initWithServer:delegate:client:))]
        fn init(this: Allocated<Self>, server: Option<&IMKServer>, delegate: Option<&AnyObject>, client: Option<&AnyObject>) -> Option<Retained<Self>> {
            let this = this.set_ivars(State::default());
            unsafe { msg_send![super(this), initWithServer:server, delegate:delegate, client:client] }
        }
        #[unsafe(method_id(menu))]
        fn command_menu(&self) -> Option<Retained<NSMenu>> {
            guarded(|| {
                let status = ipc::unix::socket_path()
                    .and_then(|path| DeadlineStream::connect(&path, MENU_STATUS_TIMEOUT))
                    .and_then(|mut stream| RuntimeStatus::query(&mut stream))
                    .unwrap_or_default();
                Some(retained_settings_menu(&self.ivars().settings_menu, self.mtm(), self, status))
            })
        }
        #[unsafe(method(kanadeOpenSettings:))]
        fn open_settings(&self, _sender: Option<&AnyObject>) {
            self.launch_settings();
        }
        #[unsafe(method(showPreferences:))]
        fn show_preferences(&self, _sender: Option<&AnyObject>) {
            self.launch_settings();
        }
        #[unsafe(method(doCommandBySelector:commandDictionary:))]
        fn command(&self, selector: Sel, command: Option<&AnyObject>) {
            settings_log(LogComponent::InputMethod, LogEvent::CommandReceived);
            guarded(|| {
                if selector == sel!(kanadeOpenSettings:) || selector == sel!(showPreferences:) {
                    self.launch_settings();
                } else {
                    settings_log(LogComponent::InputMethod, LogEvent::UnrecognizedCommand);
                    unsafe { let _: () = msg_send![super(self), doCommandBySelector:selector, commandDictionary:command]; }
                }
            });
        }
        #[unsafe(method(recognizedEvents:))]
        fn events(&self, _sender: Option<&AnyObject>) -> usize {
            NSEventMask::KeyDown.0 as usize
        }
        #[unsafe(method(activateServer:))]
        fn activate(&self, sender: Option<&AnyObject>) {
            guarded(|| {
                // An activation starts a new document context, even if the host reused its client object.
                if self.composing() {
                    incident(Incident::ActivatedWhileComposing);
                }
                self.clear_context();
                if unsafe { IsSecureEventInputEnabled() } {
                    return;
                }
                if let Some(sender) = sender && let Ok(mut stored) = self.ivars().sender.try_borrow_mut() {
                    *stored = Some(sender.retain());
                }
                self.connect();
            });
        }
        #[unsafe(method(deactivateServer:))]
        fn deactivate(&self, sender: Option<&AnyObject>) {
            guarded(|| {
                if self.composing() {
                    incident(if unsafe { IsSecureEventInputEnabled() } {
                        Incident::SecureInputWhileComposing
                    } else {
                        Incident::DeactivatedWhileComposing
                    });
                }
                if unsafe { IsSecureEventInputEnabled() } { self.clear_context(); return; }
                let client = self.ivars().engine.try_borrow_mut().ok().and_then(|mut c| c.take());
                if let (Some(sender), Some(mut client)) = (sender, client) {
                    let context = self.context(sender, &client);
                    let mut document = NativeDocument { controller:self, sender };
                    client.key(Key { special: SpecialKey::Enter as i32, ..Default::default() }, context, false, &mut document);
                    // The focus is leaving. A composition the engine could not
                    // commit ends as its reading instead of staying marked.
                    if client.is_held() { client.finish(&mut document); }
                }
                self.clear_context();
            });
        }
        #[unsafe(method(handleEvent:client:))]
        fn handle_event(&self, event: Option<&NSEvent>, sender: Option<&AnyObject>) -> bool {
            guarded(|| {
                let (Some(event), Some(sender)) = (event, sender) else { return false; };
                if event.r#type() != NSEventType::KeyDown { return false; }
                if unsafe { IsSecureEventInputEnabled() } { self.clear_context(); return false; }
                // JIS Kana is a mode key; its characters can contain a space.
                if event.keyCode() == 0x68 { return true; }
                if self.ivars().selecting_source.get() { return false; }
                if event.keyCode() == 0x66 {
                    self.switch_to_ascii(sender);
                    return true;
                }
                let Some(key) = translate(event) else { return false; };
                self.connect();
                let client = self.ivars().engine.try_borrow_mut().ok().and_then(|mut c| c.take());
                let Some(mut client) = client else { return false; };
                let context = self.context(sender, &client);
                let privacy = Privacy::try_from(context.privacy).unwrap_or(Privacy::Unknown);
                let passed = !client.would_consume(&key, privacy);
                self.stop_refinement();
                let consumed = client.key(key, context, passed, &mut NativeDocument { controller:self, sender });
                // Candidates of a lost session cannot be chosen any more.
                if client.is_held() { self.hide(); }
                if let Ok(mut stored) = self.ivars().engine.try_borrow_mut() { *stored = Some(client); }
                consumed
            })
        }
        #[unsafe(method(chooseCandidate:))]
        fn choose(&self, button: &NSButton) {
            guarded(|| {
                self.stop_refinement();
                if unsafe { IsSecureEventInputEnabled() } { self.clear_context(); return; }
                let handle = self.ivars().page.try_borrow().ok()
                    .and_then(|p| p.as_ref().and_then(|p| p.items.get(button.tag() as usize)).and_then(|c| c.handle));
                let sender = self.ivars().sender.try_borrow().ok().and_then(|s| s.clone());
                let (Some(handle), Some(sender)) = (handle, sender) else { return; };
                let client = self.ivars().engine.try_borrow_mut().ok().and_then(|mut c| c.take());
                if let Some(mut client) = client {
                    client.select(handle, &mut NativeDocument { controller:self, sender:&sender });
                    if let Ok(mut stored) = self.ivars().engine.try_borrow_mut() { *stored = Some(client); }
                }
            });
        }
        #[unsafe(method(commitComposition:))]
        fn commit_composition(&self, sender: Option<&AnyObject>) {
            guarded(|| {
                if self.composing() {
                    incident(Incident::CommitRequestedWhileComposing);
                }
                if unsafe { IsSecureEventInputEnabled() } { self.clear_context(); return; }
                let Some(sender) = sender else { return; };
                let client = self.ivars().engine.try_borrow_mut().ok().and_then(|mut c| c.take());
                if let Some(mut client) = client {
                    let context = self.context(sender, &client);
                    let mut document = NativeDocument { controller:self, sender };
                    client.key(Key { special: SpecialKey::Enter as i32, ..Default::default() }, context, false, &mut document);
                    // The host asked for the composition to end now.
                    if client.is_held() { client.finish(&mut document); }
                    if let Ok(mut stored) = self.ivars().engine.try_borrow_mut() { *stored = Some(client); }
                }
                self.hide();
            });
        }
    }
);

fn open_settings_application() {
    settings_log(LogComponent::InputMethod, LogEvent::Requested);
    let opened = guarded(|| {
        let Some(url) = NSBundle::mainBundle()
            .bundleURL()
            .URLByAppendingPathComponent(&NSString::from_str("Contents/Helpers/Settings.app"))
        else {
            return false;
        };
        let configuration = NSWorkspaceOpenConfiguration::configuration();
        configuration.setActivates(true);
        configuration.setCreatesNewApplicationInstance(false);
        let completion = block2::RcBlock::new(
            |application: *mut objc2_app_kit::NSRunningApplication,
             error: *mut objc2_foundation::NSError| {
                let event = if application.is_null() || !error.is_null() {
                    LogEvent::LaunchFailed
                } else if unsafe { &*application }.isActive() {
                    LogEvent::LaunchedActive
                } else {
                    LogEvent::LaunchedBackground
                };
                settings_log(LogComponent::InputMethod, event);
            },
        );
        NSWorkspace::sharedWorkspace().openApplicationAtURL_configuration_completionHandler(
            &url,
            &configuration,
            Some(&completion),
        );
        true
    });
    if !opened {
        settings_log(LogComponent::InputMethod, LogEvent::LaunchFailed);
        eprintln!("kanade-imk: settings could not open");
    }
}
fn guarded<R: Default>(f: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(|| {
        objc2::exception::catch(AssertUnwindSafe(f)).unwrap_or_default()
    }))
    .unwrap_or_default()
}
impl Controller {
    fn switch_to_ascii(&self, sender: &AnyObject) {
        let Ok(mut slot) = self.ivars().engine.try_borrow_mut() else {
            return;
        };
        let client = slot.take();
        drop(slot);
        if let Some(mut client) = client {
            let context = self.context(sender, &client);
            let finished = client.commit_for_source_change(
                context,
                &mut NativeDocument {
                    controller: self,
                    sender,
                },
            );
            if let Ok(mut slot) = self.ivars().engine.try_borrow_mut() {
                *slot = Some(client);
            }
            if !finished {
                return;
            }
        } else if !self
            .ivars()
            .preedit
            .try_borrow()
            .is_ok_and(|preedit| preedit.segments.is_empty())
        {
            return;
        }
        // Drop all controller borrows before TIS reenters deactivateServer:.
        self.clear_context();
        let source = unsafe { TISCopyCurrentASCIICapableKeyboardLayoutInputSource() };
        if source.is_null() {
            return;
        }
        // Limit the reentrancy guard to the synchronous selection call. IMK may
        // reuse this controller without a new activateServer: notification.
        self.ivars().selecting_source.set(true);
        let selected = guarded(|| unsafe { TISSelectInputSource(source) == 0 });
        self.ivars().selecting_source.set(false);
        unsafe { CFRelease(source) };
        if !selected {
            eprintln!("kanade-imk: ASCII input source could not be selected");
        }
    }
    fn launch_settings(&self) {
        settings_log(LogComponent::InputMethod, LogEvent::ControllerDispatched);
        open_settings_application();
    }
    fn clear_context(&self) {
        self.hide();
        if let Ok(mut engine) = self.ivars().engine.try_borrow_mut() {
            engine.take();
        }
        if let Ok(mut preedit) = self.ivars().preedit.try_borrow_mut() {
            *preedit = Preedit::default();
        }
        if let Ok(mut sender) = self.ivars().sender.try_borrow_mut() {
            sender.take();
        }
    }
    fn context(&self, sender: &AnyObject, client: &EngineClient) -> ipc::Context {
        if unsafe { IsSecureEventInputEnabled() } {
            return ipc::Context {
                privacy: Privacy::Password as i32,
                ..Default::default()
            };
        }
        let app = guarded(|| unsafe {
            let id: Option<Retained<NSString>> = msg_send![sender, bundleIdentifier];
            id.map(|s| s.to_string()).unwrap_or_default()
        });
        let privacy = client.privacy_for_app(&app);
        let mut context = ipc::Context {
            privacy: privacy as i32,
            app,
            ..Default::default()
        };
        if privacy != Privacy::Normal {
            return context;
        }
        let before = guarded(|| unsafe {
            let selected: NSRange = msg_send![sender, selectedRange];
            if selected.location == NSNotFound as usize || selected.length != 0 {
                return None;
            }
            let preedit = self.ivars().preedit.try_borrow().ok()?;
            let cursor = preedit.cursor_utf16 as usize;
            let end = selected.location.checked_sub(cursor)?;
            let surface = ipc::surface(&preedit);
            if !surface.is_empty() {
                let actual: Option<Retained<NSAttributedString>> = msg_send![sender,attributedSubstringFromRange:NSRange::new(end,ipc::utf16_len(&surface) as usize)];
                if actual?.string().to_string() != surface {
                    return None;
                }
            }
            let start = end.saturating_sub(512);
            let text: Option<Retained<NSAttributedString>> =
                msg_send![sender,attributedSubstringFromRange:NSRange::new(start,end-start)];
            Some(text?.string().to_string())
        });
        if let Some(text) = before.filter(|s| s.len() <= MAX_CONTEXT) {
            context.preceding_text = text;
            context.surrounding_available = true;
        }
        context
    }
    fn composing(&self) -> bool {
        self.ivars()
            .preedit
            .try_borrow()
            .is_ok_and(|preedit| !preedit.segments.is_empty())
    }
    /// Gives the client a connection when it has none. An existing client is
    /// kept: it may hold a composition that the new session will restore.
    fn connect(&self) {
        let Ok(mut slot) = self.ivars().engine.try_borrow_mut() else {
            return;
        };
        if slot.as_mut().is_some_and(|c| !c.needs_transport()) {
            return;
        }
        // Each attempt verifies the engine's signature on this thread. While
        // the engine is away, do not repeat that for every key.
        let ivars = self.ivars();
        if ivars
            .connect_after
            .get()
            .is_some_and(|at| Instant::now() < at)
        {
            return;
        }
        let stream = ipc::unix::socket_path()
            .ok()
            .and_then(|path| DeadlineStream::connect(&path, KEY_TIMEOUT).ok());
        let connected = match (slot.as_mut(), stream) {
            (Some(client), Some(io)) => client.reattach(io).is_ok(),
            (None, Some(io)) => {
                *slot = Client::connect(io).ok();
                slot.is_some()
            }
            (_, None) => false,
        };
        if connected {
            ivars.connect_after.set(None);
        } else {
            incident(Incident::ConnectFailed);
            ivars
                .connect_after
                .set(Some(Instant::now() + Duration::from_millis(500)));
        }
    }
    fn hide(&self) {
        self.stop_refinement();
        if let Ok(panel) = self.ivars().panel.try_borrow()
            && let Some(panel) = panel.as_ref()
        {
            panel.orderOut(None);
        }
        if let Ok(mut page) = self.ivars().page.try_borrow_mut() {
            page.take();
        }
    }
    fn show(&self, page: &CandidatePage, sender: &AnyObject) {
        if page.items.is_empty() {
            self.hide();
            return;
        }
        let mtm = self.mtm();
        let Ok(mut panel_slot) = self.ivars().panel.try_borrow_mut() else {
            return;
        };
        if panel_slot.is_none() {
            let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
                NSPanel::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(400.0, 100.0)),
                NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                NSBackingStoreType::Buffered,
                false,
            );
            panel.setFloatingPanel(true);
            panel.setBecomesKeyOnlyIfNeeded(true);
            panel.setHidesOnDeactivate(false);
            panel.setWorksWhenModal(true);
            unsafe {
                panel.setReleasedWhenClosed(false);
            }
            *panel_slot = Some(panel);
        }
        let Some(panel) = panel_slot.as_ref() else {
            return;
        };
        let (view, size) = candidates::content(page, Some(self), mtm);
        let NSSize { width, height } = size;
        panel.setContentView(Some(&view));
        let mut caret = self
            .ivars()
            .caret
            .try_borrow()
            .map(|c| *c)
            .unwrap_or_default();
        guarded(|| unsafe {
            let _: Option<Retained<AnyObject>> =
                msg_send![sender,attributesForCharacterIndex:0usize,lineHeightRectangle:&mut caret];
        });
        if let Ok(mut last) = self.ivars().caret.try_borrow_mut() {
            *last = caret;
        }
        let level = guarded(|| unsafe {
            let level: isize = msg_send![sender, windowLevel];
            level
        });
        panel.setLevel(level.saturating_add(1));
        let mut origin = NSPoint::new(caret.origin.x, caret.origin.y - height - 4.0);
        if let Some(screen) = NSScreen::mainScreen(mtm) {
            let frame = screen.visibleFrame();
            origin.x = origin
                .x
                .max(frame.origin.x)
                .min((frame.origin.x + frame.size.width - width).max(frame.origin.x));
            if origin.y < frame.origin.y {
                origin.y = caret.origin.y + caret.size.height + 4.0;
            }
            origin.y = origin
                .y
                .min((frame.origin.y + frame.size.height - height).max(frame.origin.y));
        }
        panel.setFrame_display(NSRect::new(origin, size), true);
        panel.orderFrontRegardless();
        if let Ok(mut stored) = self.ivars().page.try_borrow_mut() {
            *stored = Some(page.clone());
        }
        if let Ok(mut stored) = self.ivars().sender.try_borrow_mut() {
            *stored = Some(sender.retain());
        }
        self.schedule_refinement(page.refinement_pending);
    }
}
struct NativeDocument<'a> {
    controller: &'a Controller,
    sender: &'a AnyObject,
}
impl Document for NativeDocument<'_> {
    fn apply(&mut self, reply: &KeyReply) -> bool {
        if unsafe { IsSecureEventInputEnabled() } {
            return false;
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            objc2::exception::catch(AssertUnwindSafe(|| unsafe {
                if let Some(commit) = &reply.commit {
                    let text = NSString::from_str(&commit.value);
                    let _: () = msg_send![self.sender,insertText:&*text,replacementRange:NSRange::new(NSNotFound as usize,0)];
                }
                if let Some(preedit) = &reply.preedit {
                    let text = NSString::from_str(&ipc::surface(preedit));
                    let attributed =
                        NSAttributedString::initWithString(NSAttributedString::alloc(), &text);
                    let _: () = msg_send![self.sender,setMarkedText:&*attributed,selectionRange:NSRange::new(preedit.cursor_utf16 as usize,0),replacementRange:NSRange::new(NSNotFound as usize,0)];
                }
            }))
        }));
        if !matches!(result, Ok(Ok(()))) {
            return false;
        }
        if let Ok(mut preedit) = self.controller.ivars().preedit.try_borrow_mut() {
            *preedit = reply.preedit.clone().unwrap_or_default();
        }
        guarded(|| {
            if let Some(page) = &reply.candidates {
                self.controller.show(page, self.sender);
            } else {
                self.controller.hide();
            }
        });
        true
    }
    fn finish_reading(&mut self, reading: &str) -> bool {
        if unsafe { IsSecureEventInputEnabled() } {
            return false;
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            objc2::exception::catch(AssertUnwindSafe(|| unsafe {
                let text = NSString::from_str(reading);
                let _: () = msg_send![self.sender,insertText:&*text,replacementRange:NSRange::new(NSNotFound as usize,0)];
            }))
        }));
        guarded(|| self.controller.hide());
        if let Ok(mut preedit) = self.controller.ivars().preedit.try_borrow_mut() {
            *preedit = Preedit::default();
        }
        matches!(result, Ok(Ok(())))
    }
}
fn translate(event: &NSEvent) -> Option<Key> {
    let special = match event.keyCode() {
        36 | 76 => SpecialKey::Enter,
        49 => SpecialKey::Space,
        51 => SpecialKey::Backspace,
        53 => SpecialKey::Escape,
        123 => SpecialKey::Left,
        124 => SpecialKey::Right,
        125 => SpecialKey::Down,
        126 => SpecialKey::Up,
        48 => SpecialKey::Tab,
        117 => SpecialKey::Delete,
        115 => SpecialKey::Home,
        119 => SpecialKey::End,
        _ => SpecialKey::None,
    };
    let mut modifiers = 0;
    let flags = event.modifierFlags();
    for (flag, mask) in [
        (NSEventModifierFlags::Shift, MOD_SHIFT),
        (NSEventModifierFlags::Control, MOD_CONTROL),
        (NSEventModifierFlags::Option, MOD_ALT),
        (NSEventModifierFlags::Command, MOD_META),
    ] {
        if flags.contains(flag) {
            modifiers |= mask;
        }
    }
    if let Some(number) = match event.keyCode() {
        0x61 => Some(6),
        0x62 => Some(7),
        0x64 => Some(8),
        0x65 => Some(9),
        0x6d => Some(10),
        _ => None,
    } {
        return ipc::frontend::function_key(number, modifiers);
    }
    if special == SpecialKey::None && flags.contains(NSEventModifierFlags::Function) {
        return None;
    }
    if special == SpecialKey::None && modifiers == MOD_CONTROL {
        let key = Key {
            text: event
                .charactersIgnoringModifiers()?
                .to_string()
                .to_ascii_lowercase(),
            modifiers,
            ..Default::default()
        };
        return ipc::frontend::is_conversion_control(&key).then_some(key);
    }
    let text = if special == SpecialKey::None {
        event.characters()?.to_string()
    } else {
        String::new()
    };
    if special == SpecialKey::None && text.is_empty() {
        // A dead key produces no characters yet. The engine rejects an empty
        // key as invalid, which would end the session and its composition.
        return None;
    }
    if text
        .chars()
        .any(|c| c.is_control() || ('\u{f704}'..='\u{f726}').contains(&c))
        && special == SpecialKey::None
    {
        return None;
    }
    Some(Key {
        text,
        special: special as i32,
        modifiers,
    })
}
fn input_method_connection() -> Result<(Retained<NSString>, Retained<NSString>)> {
    let bundle = NSBundle::mainBundle();
    let id = bundle
        .bundleIdentifier()
        .context("input method bundle identifier unavailable")?;
    let name = bundle
        .objectForInfoDictionaryKey(&NSString::from_str("InputMethodConnectionName"))
        .and_then(|value| value.downcast::<NSString>().ok())
        .context("input method bundle connection name unavailable")?;
    // Modern clients resolve this bundle-scoped Mach service name. A separate
    // legacy name can work in some hosts while other hosts reject the endpoint.
    ensure!(
        name.to_string() == format!("{id}.Connection"),
        "input method connection name does not match its bundle identifier"
    );
    Ok((id, name))
}

pub fn run() -> Result<()> {
    autoreleasepool(|_| {
        let mtm = MainThreadMarker::new().context("input method requires the main thread")?;
        let (id, name) = input_method_connection()?;
        let _ = Controller::class();
        if std::env::args().any(|s| s == "--check") {
            for selector in [
                sel!(menu),
                sel!(showPreferences:),
                sel!(kanadeOpenSettings:),
                sel!(doCommandBySelector:commandDictionary:),
            ] {
                let responds: bool =
                    unsafe { msg_send![Controller::class(), instancesRespondToSelector:selector] };
                assert!(responds);
            }
            let cache = OnceCell::new();
            let target = NSObject::new();
            let menu = retained_settings_menu(&cache, mtm, &target, RuntimeStatus::default());
            let item = menu.itemAtIndex(0).unwrap();
            let dot = NSImage::imageNamed(unsafe { NSImageNameStatusAvailable })
                .context("missing status indicator")?;
            for ai in [AiState::Running, AiState::Idle, AiState::Off] {
                let status = RuntimeStatus {
                    engine_running: Some(true),
                    ai: ai as i32,
                };
                let repeated = retained_settings_menu(&cache, mtm, &target, status);
                assert!(std::ptr::eq(&*menu, &*repeated));
                assert!(std::ptr::eq(&*item, &*repeated.itemAtIndex(0).unwrap()));
                assert_eq!(repeated.numberOfItems(), 3);
                for (index, (label, running)) in (1..).zip(status.menu_rows()) {
                    let line = repeated.itemAtIndex(index).unwrap();
                    assert_eq!(line.title().to_string(), label);
                    assert_eq!(line.state() == NSControlStateValueOn, running);
                    assert_eq!(line.onStateImage().as_deref(), Some(&*dot));
                    assert!(!line.isEnabled());
                    assert_eq!(line.action(), None);
                }
            }
            assert_eq!(item.tag(), 1001);
            assert_eq!(item.action(), Some(sel!(kanadeOpenSettings:)));
            println!(
                "KanadeInputController commands, retained menu and bundle connection verified"
            );
            return Ok(());
        }
        ipc::diagnostics::start(ipc::diagnostics::Component::InputMethod, None);
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
        let server = unsafe {
            IMKServer::initWithName_bundleIdentifier(IMKServer::alloc(), Some(&name), Some(&id))
        }
        .context("input method server could not start")?;
        app.run();
        drop(server);
        Ok(())
    })
}
