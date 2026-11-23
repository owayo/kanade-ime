//! The input-mode button of the taskbar input indicator.
//!
//! Since Windows 8 the indicator shows one language bar item for a text
//! service: the first whose identifier is `GUID_LBI_INPUTMODE`. The button's
//! state is the pair of input-mode compartments, so it holds no input.
use super::*;
use std::time::{Duration, Instant};
use windows::{
    Win32::{
        Graphics::Gdi::HBITMAP,
        System::Ole::{CONNECT_E_ADVISELIMIT, CONNECT_E_CANNOTCONNECT, CONNECT_E_NOCONNECTION},
        UI::WindowsAndMessaging::*,
    },
    core::{BSTR, PCWSTR},
};
use windows_core::ComObject;

/// The button has one sink, so any nonzero value identifies it.
const COOKIE: u32 = 0x4b4e_4d42;

/// How long a keystroke relies on the taskbar colours read before it.
const COLOUR_INTERVAL: Duration = Duration::from_secs(1);

struct Face {
    glyph: &'static str,
    name: &'static str,
}
const HIRAGANA: Face = Face {
    glyph: "あ",
    name: "ひらがな",
};
const DIRECT: Face = Face {
    glyph: "A",
    name: "半角英数",
};
fn face(japanese: bool) -> &'static Face {
    if japanese { &HIRAGANA } else { &DIRECT }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Choice {
    Hiragana,
    Direct,
    Settings,
}

/// A line of the menu. A line without a choice is a separator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Entry {
    pub choice: Option<Choice>,
    pub label: &'static str,
    pub checked: bool,
    pub enabled: bool,
}

/// The menu of the button. `settings` tells whether this host may start the
/// settings application.
pub(super) fn menu(japanese: bool, settings: bool) -> [Entry; 4] {
    let mode = |choice, face: &'static Face, checked| Entry {
        choice: Some(choice),
        label: face.name,
        checked,
        enabled: true,
    };
    [
        mode(Choice::Hiragana, &HIRAGANA, japanese),
        mode(Choice::Direct, &DIRECT, !japanese),
        Entry {
            choice: None,
            label: "",
            checked: false,
            enabled: false,
        },
        Entry {
            choice: Some(Choice::Settings),
            label: "Kanade の設定…",
            checked: false,
            enabled: settings,
        },
    ]
}

fn item_info() -> TF_LANGBARITEMINFO {
    let mut info = TF_LANGBARITEMINFO {
        clsidService: CLSID,
        guidItem: GUID_LBI_INPUTMODE,
        // A button receives both clicks. The second style keeps the item on
        // a minimised language bar of the desktop.
        dwStyle: TF_LBI_STYLE_BTN_BUTTON | TF_LBI_STYLE_SHOWNINTRAY,
        ..Default::default()
    };
    // The last unit stays zero: the description is a terminated string.
    for (slot, unit) in info
        .szDescription
        .iter_mut()
        .take(TF_LBI_DESC_MAXLEN as usize - 1)
        .zip("Kanade 入力モード".encode_utf16())
    {
        *slot = unit;
    }
    info
}

/// Shows the menu at the indicator and returns the chosen line. This runs a
/// message loop, so the text service can be deactivated before it returns.
fn popup(entries: &[Entry], at: POINT, exclude: Option<RECT>) -> Option<Choice> {
    unsafe {
        // The menu needs a window of this thread. The input focus stays where
        // it is; no command message is sent to that window.
        let owner = [GetFocus(), GetActiveWindow()]
            .into_iter()
            .find(|window| !window.is_invalid())?;
        let menu = CreatePopupMenu().ok()?;
        let mut built = true;
        for (id, entry) in (1usize..).zip(entries) {
            let added = if entry.choice.is_none() {
                AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null())
            } else {
                let label: Vec<u16> = entry.label.encode_utf16().chain(Some(0)).collect();
                let state = if entry.enabled { MF_ENABLED } else { MF_GRAYED };
                AppendMenuW(menu, MF_STRING | state, id, PCWSTR(label.as_ptr()))
            };
            built &= added.is_ok();
            if entry.checked {
                let id = id as u32;
                built &= CheckMenuRadioItem(menu, id, id, id, MF_BYCOMMAND.0).is_ok();
            }
        }
        let chosen = if built {
            let avoid = exclude.map(|area| TPMPARAMS {
                cbSize: std::mem::size_of::<TPMPARAMS>() as u32,
                rcExclude: area,
            });
            let flags =
                TPM_NONOTIFY | TPM_RETURNCMD | TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_VERTICAL;
            TrackPopupMenuEx(
                menu,
                flags.0,
                at.x,
                at.y,
                owner,
                avoid.as_ref().map(|avoid| avoid as *const TPMPARAMS),
            )
            .0
        } else {
            0
        };
        let _ = DestroyMenu(menu);
        entries
            .get(usize::try_from(chosen).ok()?.checked_sub(1)?)?
            .choice
    }
}

/// How the button reaches the settings application.
pub(super) struct Settings {
    /// Whether the menu offers the settings now.
    pub available: Box<dyn Fn() -> bool>,
    pub open: Box<dyn Fn() -> Result<()>>,
}

impl Settings {
    /// The installed settings application, when this host may start it.
    /// `secure` is the activation flag of the secure desktop.
    pub(super) fn of_host(secure: bool) -> Self {
        let allowed = programs::host_may_launch(secure);
        Self {
            available: Box::new(move || allowed && programs::settings_installed()),
            open: Box::new(programs::open_settings),
        }
    }
}

#[implement(ITfLangBarItemButton, ITfSource)]
pub(super) struct ModeButton {
    /// Empty once the text service is deactivated.
    switch: RefCell<Option<input_mode::Switch>>,
    tid: u32,
    settings: Settings,
    sink: RefCell<Option<ITfLangBarItemSink>>,
    /// Reads the colour of the taskbar text.
    colours: Box<dyn Fn() -> mode_icon::Palette>,
    /// The mode and the colours the sink was last told about.
    shown: Cell<Option<(bool, mode_icon::Palette)>>,
    /// When the colours were last read.
    read: Cell<Instant>,
    /// The pictures drawn so far, by mode, icon size and colours. Drawing one
    /// takes longer than a keystroke should wait for the indicator.
    drawn: RefCell<Vec<(Look, mode_icon::Drawing)>>,
    _guard: ModuleGuard,
}

/// What decides the picture of the button: the mode, the size and the colours.
type Look = (bool, i32, mode_icon::Palette);

/// More looks than one thread shows: two modes at a few sizes and themes.
const DRAWN_LIMIT: usize = 8;

impl ModeButton {
    fn new(switch: input_mode::Switch, tid: u32, settings: Settings) -> Self {
        let colours = Box::new(mode_icon::Palette::of_taskbar);
        Self::with_colours(switch, tid, settings, colours)
    }

    fn with_colours(
        switch: input_mode::Switch,
        tid: u32,
        settings: Settings,
        colours: Box<dyn Fn() -> mode_icon::Palette>,
    ) -> Self {
        let shown = (switch.japanese(), colours());
        Self {
            switch: RefCell::new(Some(switch)),
            tid,
            settings,
            sink: RefCell::new(None),
            colours,
            shown: Cell::new(Some(shown)),
            read: Cell::new(Instant::now()),
            drawn: RefCell::new(Vec::new()),
            _guard: ModuleGuard::new(),
        }
    }

    /// The picture of the current mode, drawn once for each look.
    fn drawing(&self) -> Result<mode_icon::Drawing> {
        let look: Look = (
            self.japanese()?,
            mode_icon::small_icon_size(),
            (self.colours)(),
        );
        let kept = self.drawn.try_borrow().ok().and_then(|drawn| {
            drawn
                .iter()
                .find(|(kept, _)| *kept == look)
                .map(|(_, drawing)| drawing.clone())
        });
        if let Some(drawing) = kept {
            return Ok(drawing);
        }
        let drawing = mode_icon::Drawing::new(face(look.0).glyph, look.1, look.2)?;
        if let Ok(mut drawn) = self.drawn.try_borrow_mut() {
            if drawn.len() >= DRAWN_LIMIT {
                drawn.clear();
            }
            drawn.push((look, drawing.clone()));
        }
        Ok(drawing)
    }

    fn switch(&self) -> Result<input_mode::Switch> {
        self.switch
            .try_borrow()
            .ok()
            .and_then(|switch| switch.clone())
            .ok_or_else(failure)
    }

    fn japanese(&self) -> Result<bool> {
        Ok(self.switch()?.japanese())
    }

    fn entries(&self) -> Result<[Entry; 4]> {
        Ok(menu(self.japanese()?, (self.settings.available)()))
    }

    /// The compartment change reaches the text service, which calls `sync`.
    fn apply(&self, command: input_mode::Command) -> Result<()> {
        self.switch()?.apply(self.tid, command)
    }

    fn perform(&self, choice: Choice) -> Result<()> {
        match choice {
            Choice::Hiragana => self.apply(input_mode::Command::On),
            Choice::Direct => self.apply(input_mode::Command::Off),
            Choice::Settings if (self.settings.available)() => (self.settings.open)(),
            Choice::Settings => Err(E_ACCESSDENIED.into()),
        }
    }

    /// Tells the indicator when the mode or the taskbar colours have changed.
    pub(super) fn sync(&self) {
        self.sync_at(Instant::now());
    }

    fn sync_at(&self, time: Instant) {
        let Ok(japanese) = self.japanese() else {
            return;
        };
        self.read.set(time);
        let now = (japanese, (self.colours)());
        if self.shown.replace(Some(now)) == Some(now) {
            return;
        }
        // The sink asks for the icon and the text again before it returns.
        let sink = self.sink.try_borrow().ok().and_then(|sink| sink.clone());
        if let Some(sink) = sink {
            let _ = unsafe { sink.OnUpdate(TF_LBI_ICON | TF_LBI_TEXT | TF_LBI_TOOLTIP) };
        }
    }

    /// Called for every key. The taskbar theme can change while this thread
    /// keeps the focus, and nothing reports that to a text service. The icon
    /// has no outline to stay legible on the other theme, so the colours are
    /// read again, at most once in `COLOUR_INTERVAL`.
    pub(super) fn refresh(&self) {
        self.refresh_at(Instant::now());
    }

    fn refresh_at(&self, time: Instant) {
        if time.saturating_duration_since(self.read.get()) >= COLOUR_INTERVAL {
            self.sync_at(time);
        }
    }

    fn detach(&self) {
        if let Ok(mut switch) = self.switch.try_borrow_mut() {
            switch.take();
        }
        if let Ok(mut sink) = self.sink.try_borrow_mut() {
            sink.take();
        }
    }
}

impl ITfLangBarItem_Impl for ModeButton_Impl {
    fn GetInfo(&self, info: *mut TF_LANGBARITEMINFO) -> Result<()> {
        let info = unsafe { info.as_mut() }.ok_or_else(|| Error::from(E_INVALIDARG))?;
        *info = item_info();
        Ok(())
    }
    fn GetStatus(&self) -> Result<u32> {
        Ok(0)
    }
    fn Show(&self, _: BOOL) -> Result<()> {
        // The indicator owns the visibility of the button.
        Err(E_NOTIMPL.into())
    }
    fn GetTooltipString(&self) -> Result<BSTR> {
        Ok(format!(
            "Kanade: {}（右クリックでメニュー）",
            face(self.japanese()?).name
        )
        .as_str()
        .into())
    }
}

impl ITfLangBarItemButton_Impl for ModeButton_Impl {
    fn OnClick(&self, click: TfLBIClick, at: &POINT, area: *const RECT) -> Result<()> {
        if click == TF_LBI_CLK_LEFT {
            return self.apply(input_mode::Command::Toggle);
        }
        if click != TF_LBI_CLK_RIGHT {
            return Ok(());
        }
        let entries = self.entries()?;
        match popup(&entries, *at, unsafe { area.as_ref() }.copied()) {
            Some(choice) => self.perform(choice),
            None => Ok(()),
        }
    }
    // A host that presents the button as a menu gets the same lines.
    fn InitMenu(&self, menu: Ref<ITfMenu>) -> Result<()> {
        let menu = menu.ok()?;
        for (id, entry) in (0u32..).zip(self.entries()?) {
            let flags = match entry {
                Entry { choice: None, .. } => TF_LBMENUF_SEPARATOR,
                Entry { enabled: false, .. } => TF_LBMENUF_GRAYED,
                Entry { checked: true, .. } => TF_LBMENUF_RADIOCHECKED,
                _ => 0,
            };
            let label: Vec<u16> = entry.label.encode_utf16().collect();
            unsafe {
                menu.AddMenuItem(
                    id,
                    flags,
                    HBITMAP::default(),
                    HBITMAP::default(),
                    &label,
                    std::ptr::null_mut(),
                )?;
            }
        }
        Ok(())
    }
    fn OnMenuSelect(&self, id: u32) -> Result<()> {
        match self.entries()?.get(id as usize) {
            Some(Entry {
                choice: Some(choice),
                enabled: true,
                ..
            }) => self.perform(*choice),
            _ => Err(E_INVALIDARG.into()),
        }
    }
    fn GetIcon(&self) -> Result<HICON> {
        self.drawing()?.icon()
    }
    fn GetText(&self) -> Result<BSTR> {
        Ok(face(self.japanese()?).name.into())
    }
}

impl ITfSource_Impl for ModeButton_Impl {
    fn AdviseSink(&self, iid: *const GUID, sink: Ref<IUnknown>) -> Result<u32> {
        if unsafe { iid.as_ref() } != Some(&ITfLangBarItemSink::IID) {
            return Err(CONNECT_E_CANNOTCONNECT.into());
        }
        let sink = sink
            .ok()?
            .cast::<ITfLangBarItemSink>()
            .map_err(|_| Error::from(E_INVALIDARG))?;
        let mut slot = self.sink.try_borrow_mut().map_err(|_| failure())?;
        if slot.is_some() {
            return Err(CONNECT_E_ADVISELIMIT.into());
        }
        *slot = Some(sink);
        Ok(COOKIE)
    }
    fn UnadviseSink(&self, cookie: u32) -> Result<()> {
        if cookie != COOKIE {
            return Err(E_INVALIDARG.into());
        }
        self.sink
            .try_borrow_mut()
            .map_err(|_| failure())?
            .take()
            .map(drop)
            .ok_or_else(|| Error::from(CONNECT_E_NOCONNECTION))
    }
}

/// The button while it is an item of the thread's language bar.
pub(super) struct Item {
    items: ITfLangBarItemMgr,
    item: ITfLangBarItem,
    button: ComObject<ModeButton>,
}

impl Item {
    pub(super) fn add(
        manager: &ITfThreadMgr,
        tid: u32,
        switch: input_mode::Switch,
        settings: Settings,
    ) -> Result<Self> {
        let items = manager.cast::<ITfLangBarItemMgr>()?;
        let button = ComObject::new(ModeButton::new(switch, tid, settings));
        let item = ITfLangBarItem::from(button.to_interface::<ITfLangBarItemButton>());
        unsafe { items.AddItem(&item)? };
        Ok(Self {
            items,
            item,
            button,
        })
    }

    pub(super) fn button(&self) -> ComObject<ModeButton> {
        self.button.clone()
    }
}

impl Drop for Item {
    fn drop(&mut self) {
        let _ = unsafe { self.items.RemoveItem(&self.item) };
        self.button.detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_core::OutRef;

    const CHANGED: u32 = TF_LBI_ICON | TF_LBI_TEXT | TF_LBI_TOOLTIP;

    #[implement(ITfLangBarItemSink)]
    struct Updates(Rc<RefCell<Vec<u32>>>);
    impl ITfLangBarItemSink_Impl for Updates_Impl {
        fn OnUpdate(&self, flags: u32) -> Result<()> {
            self.0.try_borrow_mut().map_err(|_| failure())?.push(flags);
            Ok(())
        }
    }

    #[implement(ITfMenu)]
    struct Lines(Rc<RefCell<Vec<(u32, u32, String)>>>);
    impl ITfMenu_Impl for Lines_Impl {
        fn AddMenuItem(
            &self,
            id: u32,
            flags: u32,
            _: HBITMAP,
            _: HBITMAP,
            text: &PCWSTR,
            len: u32,
            _: OutRef<ITfMenu>,
        ) -> Result<()> {
            let text = if len == 0 {
                String::new()
            } else {
                String::from_utf16_lossy(unsafe {
                    std::slice::from_raw_parts(text.0, len as usize)
                })
            };
            self.0
                .try_borrow_mut()
                .map_err(|_| failure())?
                .push((id, flags, text));
            Ok(())
        }
    }

    /// A settings application that counts its openings instead of starting.
    fn settings(available: &Rc<Cell<bool>>, opened: &Rc<Cell<u32>>) -> Settings {
        let (available, opened) = (available.clone(), opened.clone());
        Settings {
            available: Box::new(move || available.get()),
            open: Box::new(move || {
                opened.set(opened.get() + 1);
                Ok(())
            }),
        }
    }

    fn code<T>(result: Result<T>) -> Option<HRESULT> {
        result.err().map(|error| error.code())
    }

    /// Starts TSF on this thread without the text services of the computer.
    /// An installed Kanade would otherwise be active here as well, with its
    /// own button and under the same client identifier.
    unsafe fn activate(thread: &ITfThreadMgr) -> Result<u32> {
        let mut application = 0;
        unsafe {
            thread
                .cast::<ITfThreadMgrEx>()?
                .ActivateEx(&mut application, TF_TMAE_NOACTIVATETIP)?;
        }
        Ok(application)
    }

    /// Runs `check` on a new TSF thread with a text service whose input mode
    /// is attached to the thread's real compartments, starting in Japanese.
    fn with_tip(
        check: impl FnOnce(&ComObject<Tip>, &ITfThreadMgr, u32, input_mode::Switch) -> Result<()>,
    ) -> Result<()> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
        let result = (|| unsafe {
            let thread: ITfThreadMgr =
                CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
            let tid = activate(&thread)?;
            let tip = ComObject::new(Tip::new());
            tip.shared.tid.set(tid);
            let mode = input_mode::Mode::attach(&thread, tid, &tip.to_interface())?;
            let switch = mode.switch();
            *tip.mode.borrow_mut() = Some(mode);
            let result = switch
                .apply(tid, input_mode::Command::On)
                .and_then(|()| check(&tip, &thread, tid, switch));
            drop(tip.button.borrow_mut().take());
            drop(tip.mode.borrow_mut().take());
            thread.Deactivate()?;
            result
        })();
        unsafe { CoUninitialize() };
        result
    }

    #[test]
    fn the_menu_marks_the_mode_and_offers_the_settings_when_they_can_open() {
        let lines = |japanese, settings| {
            menu(japanese, settings)
                .map(|line| (line.choice, line.label, line.checked, line.enabled))
        };
        assert_eq!(
            lines(true, true),
            [
                (Some(Choice::Hiragana), "ひらがな", true, true),
                (Some(Choice::Direct), "半角英数", false, true),
                (None, "", false, false),
                (Some(Choice::Settings), "Kanade の設定…", false, true),
            ]
        );
        assert_eq!(
            lines(false, false),
            [
                (Some(Choice::Hiragana), "ひらがな", false, true),
                (Some(Choice::Direct), "半角英数", true, true),
                (None, "", false, false),
                (Some(Choice::Settings), "Kanade の設定…", false, false),
            ]
        );
    }

    #[test]
    fn the_button_is_the_input_mode_item_of_its_thread() -> Result<()> {
        with_tip(|_, thread, tid, switch| unsafe {
            let (available, opened) = (Rc::new(Cell::new(false)), Rc::new(Cell::new(0)));
            let installed = Item::add(thread, tid, switch, settings(&available, &opened))?;
            let items = thread.cast::<ITfLangBarItemMgr>()?;
            let item = items.GetItem(&GUID_LBI_INPUTMODE)?;
            let mut info = TF_LANGBARITEMINFO::default();
            item.GetInfo(&mut info)?;
            assert_eq!(info.clsidService, CLSID);
            assert_eq!(info.guidItem, GUID_LBI_INPUTMODE);
            assert_eq!(
                info.dwStyle,
                TF_LBI_STYLE_BTN_BUTTON | TF_LBI_STYLE_SHOWNINTRAY
            );
            let description: Vec<u16> = info
                .szDescription
                .into_iter()
                .take_while(|unit| *unit != 0)
                .collect();
            assert_eq!(String::from_utf16_lossy(&description), "Kanade 入力モード");
            assert_eq!(info.szDescription.last(), Some(&0));
            assert_eq!(code(item.GetInfo(std::ptr::null_mut())), Some(E_INVALIDARG));
            assert_eq!(item.GetStatus()?, 0);
            assert_eq!(code(item.Show(true)), Some(E_NOTIMPL));
            let button = item.cast::<ITfLangBarItemButton>()?;
            assert_eq!(button.GetText()?.to_string(), "ひらがな");
            assert_eq!(
                item.GetTooltipString()?.to_string(),
                "Kanade: ひらがな（右クリックでメニュー）"
            );
            let icon = button.GetIcon()?;
            assert!(!icon.is_invalid());
            DestroyIcon(icon)?;
            // A click of another kind does nothing.
            button.OnClick(TfLBIClick(0), POINT::default(), std::ptr::null())?;
            assert_eq!(button.GetText()?.to_string(), "ひらがな");
            // The deactivated text service leaves no item and no state behind.
            drop(installed);
            assert!(items.GetItem(&GUID_LBI_INPUTMODE).is_err());
            assert!(button.GetText().is_err());
            assert!(button.GetIcon().is_err());
            assert!(
                button
                    .OnClick(TF_LBI_CLK_LEFT, POINT::default(), std::ptr::null())
                    .is_err()
            );
            assert!(button.OnMenuSelect(0).is_err());
            Ok(())
        })
    }

    #[test]
    fn a_click_switches_the_mode_and_tells_the_indicator() -> Result<()> {
        with_tip(|tip, thread, tid, switch| unsafe {
            let (available, opened) = (Rc::new(Cell::new(false)), Rc::new(Cell::new(0)));
            let installed = Item::add(thread, tid, switch, settings(&available, &opened))?;
            let button = installed.button();
            *tip.button.borrow_mut() = Some(installed);
            let item = button.to_interface::<ITfLangBarItemButton>();
            // Listen where the indicator does: at the thread's language bar.
            let updates = Rc::new(RefCell::new(Vec::new()));
            let sink: ITfLangBarItemSink = Updates(updates.clone()).into();
            let items = thread.cast::<ITfLangBarItemMgr>()?;
            let mut cookie = 0;
            items.AdviseItemSink(&sink, &mut cookie, &GUID_LBI_INPUTMODE)?;
            assert!(tip.shared.japanese.get());
            item.OnClick(TF_LBI_CLK_LEFT, POINT::default(), std::ptr::null())?;
            assert!(!tip.shared.japanese.get());
            assert_eq!(item.GetText()?.to_string(), "半角英数");
            assert_eq!(
                item.GetTooltipString()?.to_string(),
                "Kanade: 半角英数（右クリックでメニュー）"
            );
            assert_eq!(*updates.borrow(), [CHANGED]);
            item.OnClick(TF_LBI_CLK_LEFT, POINT::default(), std::ptr::null())?;
            assert!(tip.shared.japanese.get());
            assert_eq!(item.GetText()?.to_string(), "ひらがな");
            assert_eq!(*updates.borrow(), [CHANGED, CHANGED]);
            // A key that switches the mode updates the button as well.
            let keys = tip.to_interface::<ITfKeyEventSink>();
            assert!(keys.OnKeyDown(None, WPARAM(0x1a), LPARAM(0))?.as_bool());
            assert_eq!(item.GetText()?.to_string(), "半角英数");
            assert_eq!(updates.borrow().len(), 3);
            // Regaining the focus without a change tells the indicator nothing.
            keys.OnSetFocus(true)?;
            assert_eq!(updates.borrow().len(), 3);
            items.UnadviseItemSink(cookie)?;
            Ok(())
        })
    }

    #[test]
    fn the_button_accepts_one_item_sink() -> Result<()> {
        with_tip(|_, _, tid, switch| unsafe {
            let (available, opened) = (Rc::new(Cell::new(false)), Rc::new(Cell::new(0)));
            let button = ComObject::new(ModeButton::new(
                switch.clone(),
                tid,
                settings(&available, &opened),
            ));
            let source = button.to_interface::<ITfSource>();
            let updates = Rc::new(RefCell::new(Vec::new()));
            let sink: ITfLangBarItemSink = Updates(updates.clone()).into();
            assert_eq!(
                code(source.AdviseSink(&ITfThreadMgrEventSink::IID, &sink)),
                Some(CONNECT_E_CANNOTCONNECT)
            );
            assert_eq!(
                code(source.AdviseSink(std::ptr::null(), &sink)),
                Some(CONNECT_E_CANNOTCONNECT)
            );
            let cookie = source.AdviseSink(&ITfLangBarItemSink::IID, &sink)?;
            assert_eq!(
                code(source.AdviseSink(&ITfLangBarItemSink::IID, &sink)),
                Some(CONNECT_E_ADVISELIMIT)
            );
            // The sink hears of a change once, and of nothing otherwise.
            button.sync();
            assert!(updates.borrow().is_empty());
            switch.apply(tid, input_mode::Command::Off)?;
            button.sync();
            button.sync();
            assert_eq!(*updates.borrow(), [CHANGED]);
            assert_eq!(code(source.UnadviseSink(cookie + 1)), Some(E_INVALIDARG));
            source.UnadviseSink(cookie)?;
            assert_eq!(
                code(source.UnadviseSink(cookie)),
                Some(CONNECT_E_NOCONNECTION)
            );
            // A change without a sink is not an error.
            switch.apply(tid, input_mode::Command::On)?;
            button.sync();
            assert_eq!(updates.borrow().len(), 1);
            // The released button forgets a sink that was never removed.
            source.AdviseSink(&ITfLangBarItemSink::IID, &sink)?;
            button.detach();
            switch.apply(tid, input_mode::Command::Off)?;
            button.sync();
            assert_eq!(updates.borrow().len(), 1);
            Ok(())
        })
    }

    #[test]
    fn a_theme_change_reaches_the_indicator_with_the_next_key() -> Result<()> {
        with_tip(|_, _, tid, switch| unsafe {
            let (available, opened) = (Rc::new(Cell::new(false)), Rc::new(Cell::new(0)));
            let theme = Rc::new(Cell::new(mode_icon::Palette::ON_DARK));
            let reads = Rc::new(Cell::new(0u32));
            let colours = {
                let (theme, reads) = (theme.clone(), reads.clone());
                move || {
                    reads.set(reads.get() + 1);
                    theme.get()
                }
            };
            let button = ComObject::new(ModeButton::with_colours(
                switch,
                tid,
                settings(&available, &opened),
                Box::new(colours),
            ));
            let updates = Rc::new(RefCell::new(Vec::new()));
            let sink: ITfLangBarItemSink = Updates(updates.clone()).into();
            let source = button.to_interface::<ITfSource>();
            source.AdviseSink(&ITfLangBarItemSink::IID, &sink)?;
            let made = button.read.get();
            assert_eq!(reads.get(), 1);
            // The taskbar turns light while this thread keeps the focus. Keys
            // soon after the last reading rely on it.
            theme.set(mode_icon::Palette::ON_LIGHT);
            button.refresh_at(made);
            button.refresh_at(made + Duration::from_millis(900));
            assert_eq!(reads.get(), 1);
            assert!(updates.borrow().is_empty());
            // A later key reads the colours again and the indicator redraws.
            button.refresh_at(made + Duration::from_secs(2));
            assert_eq!(reads.get(), 2);
            assert_eq!(*updates.borrow(), [CHANGED]);
            // The icon is drawn in the colours of that moment.
            let icon = button.to_interface::<ITfLangBarItemButton>().GetIcon()?;
            DestroyIcon(icon)?;
            assert_eq!(reads.get(), 3);
            assert_eq!(
                button.drawn.borrow().first().map(|(look, _)| look.2),
                Some(mode_icon::Palette::ON_LIGHT)
            );
            // Further keys within the interval, and a clock that runs
            // backwards, read nothing.
            button.refresh_at(made + Duration::from_millis(2900));
            button.refresh_at(made);
            assert_eq!(reads.get(), 3);
            // An unchanged theme is read again later without a redraw.
            button.refresh_at(made + Duration::from_secs(4));
            assert_eq!(reads.get(), 4);
            assert_eq!(updates.borrow().len(), 1);
            // A released button reads nothing.
            button.detach();
            button.refresh_at(made + Duration::from_secs(9));
            assert_eq!(reads.get(), 4);
            Ok(())
        })
    }

    #[test]
    fn the_button_draws_each_look_once() -> Result<()> {
        with_tip(|_, _, tid, switch| unsafe {
            let (available, opened) = (Rc::new(Cell::new(false)), Rc::new(Cell::new(0)));
            let theme = Rc::new(Cell::new(mode_icon::Palette::ON_DARK));
            let colours = {
                let theme = theme.clone();
                move || theme.get()
            };
            let button = ComObject::new(ModeButton::with_colours(
                switch.clone(),
                tid,
                settings(&available, &opened),
                Box::new(colours),
            ));
            let item = button.to_interface::<ITfLangBarItemButton>();
            let looks = || -> Vec<(bool, mode_icon::Palette)> {
                let drawn = button.drawn.borrow();
                drawn.iter().map(|(look, _)| (look.0, look.2)).collect()
            };
            // Nothing is drawn before the indicator asks.
            assert!(looks().is_empty());
            DestroyIcon(item.GetIcon()?)?;
            let first = button.drawn.borrow().clone();
            assert_eq!(looks(), [(true, mode_icon::Palette::ON_DARK)]);
            // The same look again is the kept picture.
            DestroyIcon(item.GetIcon()?)?;
            assert_eq!(*button.drawn.borrow(), first);
            // The other mode and the other theme are pictures of their own.
            switch.apply(tid, input_mode::Command::Off)?;
            DestroyIcon(item.GetIcon()?)?;
            theme.set(mode_icon::Palette::ON_LIGHT);
            DestroyIcon(item.GetIcon()?)?;
            assert_eq!(
                looks(),
                [
                    (true, mode_icon::Palette::ON_DARK),
                    (false, mode_icon::Palette::ON_DARK),
                    (false, mode_icon::Palette::ON_LIGHT),
                ]
            );
            assert_ne!(button.drawn.borrow().first(), button.drawn.borrow().get(1));
            // Looks beyond the limit start the collection again.
            for shade in 1..=DRAWN_LIMIT as u8 {
                theme.set(mode_icon::Palette {
                    glyph: [shade, 0, 0],
                    opacity: 255,
                });
                DestroyIcon(item.GetIcon()?)?;
                assert!(button.drawn.borrow().len() <= DRAWN_LIMIT);
            }
            assert_eq!(looks().len(), 3);
            assert_eq!(looks().last().map(|look| look.1.glyph), Some([8, 0, 0]));
            Ok(())
        })
    }

    #[test]
    fn the_menu_switches_modes_and_opens_the_settings() -> Result<()> {
        with_tip(|tip, thread, tid, switch| unsafe {
            let (available, opened) = (Rc::new(Cell::new(true)), Rc::new(Cell::new(0)));
            let installed = Item::add(thread, tid, switch, settings(&available, &opened))?;
            let item = installed.button().to_interface::<ITfLangBarItemButton>();
            *tip.button.borrow_mut() = Some(installed);
            let lines = Rc::new(RefCell::new(Vec::new()));
            let menu: ITfMenu = Lines(lines.clone()).into();
            item.InitMenu(&menu)?;
            assert_eq!(
                *lines.borrow(),
                [
                    (0, TF_LBMENUF_RADIOCHECKED, "ひらがな".to_owned()),
                    (1, 0, "半角英数".to_owned()),
                    (2, TF_LBMENUF_SEPARATOR, String::new()),
                    (3, 0, "Kanade の設定…".to_owned()),
                ]
            );
            item.OnMenuSelect(1)?;
            assert!(!tip.shared.japanese.get());
            item.OnMenuSelect(0)?;
            assert!(tip.shared.japanese.get());
            // Choosing the current mode keeps it.
            item.OnMenuSelect(0)?;
            assert!(tip.shared.japanese.get());
            item.OnMenuSelect(3)?;
            assert_eq!(opened.get(), 1);
            // The separator and a line that does not exist are not commands.
            assert_eq!(code(item.OnMenuSelect(2)), Some(E_INVALIDARG));
            assert_eq!(code(item.OnMenuSelect(4)), Some(E_INVALIDARG));
            // A host that may not start the settings shows the line greyed.
            available.set(false);
            item.OnMenuSelect(1)?;
            lines.borrow_mut().clear();
            item.InitMenu(&menu)?;
            assert_eq!(
                *lines.borrow(),
                [
                    (0, 0, "ひらがな".to_owned()),
                    (1, TF_LBMENUF_RADIOCHECKED, "半角英数".to_owned()),
                    (2, TF_LBMENUF_SEPARATOR, String::new()),
                    (3, TF_LBMENUF_GRAYED, "Kanade の設定…".to_owned()),
                ]
            );
            assert_eq!(code(item.OnMenuSelect(3)), Some(E_INVALIDARG));
            assert_eq!(opened.get(), 1);
            Ok(())
        })
    }

    // TSF accepts a key sink only from a text service it activated itself, so
    // activation is covered by the registered-profile test.
    #[test]
    fn deactivating_the_text_service_removes_the_button() -> Result<()> {
        with_tip(|tip, thread, tid, switch| unsafe {
            let (available, opened) = (Rc::new(Cell::new(false)), Rc::new(Cell::new(0)));
            let installed = Item::add(thread, tid, switch, settings(&available, &opened))?;
            *tip.button.borrow_mut() = Some(installed);
            let items = thread.cast::<ITfLangBarItemMgr>()?;
            let button = items
                .GetItem(&GUID_LBI_INPUTMODE)?
                .cast::<ITfLangBarItemButton>()?;
            tip.to_interface::<ITfTextInputProcessorEx>().Deactivate()?;
            assert!(items.GetItem(&GUID_LBI_INPUTMODE).is_err());
            assert!(button.GetText().is_err());
            Ok(())
        })
    }

    #[test]
    fn the_settings_of_a_test_host_are_not_installed() {
        // The test program has no settings application beside it, and the
        // secure desktop never offers one.
        assert!(!(Settings::of_host(false).available)());
        assert!(!(Settings::of_host(true).available)());
    }
}
