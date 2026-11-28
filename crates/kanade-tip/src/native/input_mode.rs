use super::*;
use windows::Win32::System::Variant::{VARIANT, VT_I4};

const JAPANESE: u32 =
    TF_CONVERSIONMODE_NATIVE | TF_CONVERSIONMODE_FULLSHAPE | TF_CONVERSIONMODE_ROMAN;

#[derive(Clone, Copy)]
pub(super) enum Command {
    Toggle,
    On,
    Off,
}

impl Command {
    pub(super) fn japanese(self, current: bool) -> bool {
        match self {
            Self::On => true,
            Self::Off => false,
            Self::Toggle => !current,
        }
    }
}

struct Binding {
    id: GUID,
    key: TF_PRESERVEDKEY,
    command: Command,
}

const fn key(vk: u32, modifiers: u32, command: Command) -> Binding {
    Binding {
        id: GUID::from_u128(0x8a1c7f52_9027_4b3d_833d_c2bbf3100000 + vk as u128),
        key: TF_PRESERVEDKEY {
            uVKey: vk,
            uModifiers: modifiers,
        },
        command,
    }
}

const KEYS: &[Binding] = &[
    key(0x19, TF_MOD_IGNORE_ALL_MODIFIER, Command::Toggle), // VK_KANJI
    key(0xf3, TF_MOD_IGNORE_ALL_MODIFIER, Command::Toggle), // JIS half/full width
    key(0xf4, TF_MOD_IGNORE_ALL_MODIFIER, Command::Toggle),
    key(0x16, TF_MOD_IGNORE_ALL_MODIFIER, Command::On), // VK_IME_ON
    key(0x1a, TF_MOD_IGNORE_ALL_MODIFIER, Command::Off), // VK_IME_OFF
    key(0xc0, TF_MOD_ALT, Command::Toggle),             // US Alt + grave
];

pub(super) fn physical(vk: usize) -> Option<Command> {
    KEYS.iter()
        .find(|b| {
            b.key.uVKey as usize == vk
                && (b.key.uModifiers != TF_MOD_ALT
                    || (key_down(VK_MENU) && !key_down(VK_CONTROL) && !key_down(VK_SHIFT)))
        })
        .map(|b| b.command)
}

pub(super) fn preserved(id: &GUID) -> Option<Command> {
    KEYS.iter().find(|b| b.id == *id).map(|b| b.command)
}

/// The two thread compartments that hold the input mode. Keys and the taskbar
/// button both read and write the mode through a copy of this.
#[derive(Clone)]
pub(super) struct Switch {
    open: ITfCompartment,
    conversion: ITfCompartment,
}

pub(super) struct Mode {
    switch: Switch,
    changes: Vec<(ITfSource, u32)>,
    keys: ITfKeystrokeMgr,
    registered: Vec<&'static Binding>,
}

fn value(slot: &ITfCompartment) -> Option<i32> {
    let v = unsafe { slot.GetValue() }.ok()?;
    // TSF returns VT_EMPTY while a compartment is uninitialised.
    unsafe {
        let inner = &v.Anonymous.Anonymous;
        (inner.vt == VT_I4).then(|| inner.Anonymous.lVal)
    }
}

impl Switch {
    pub(super) fn japanese(&self) -> bool {
        value(&self.open).is_some_and(|v| v != 0)
            && value(&self.conversion).is_some_and(|v| v as u32 & TF_CONVERSIONMODE_NATIVE != 0)
    }

    pub(super) fn apply(&self, tid: u32, command: Command) -> Result<()> {
        let on = command.japanese(self.japanese());
        if on && !value(&self.conversion).is_some_and(|v| v as u32 & TF_CONVERSIONMODE_NATIVE != 0)
        {
            unsafe {
                self.conversion
                    .SetValue(tid, &VARIANT::from(JAPANESE as i32))?
            };
        }
        unsafe { self.open.SetValue(tid, &VARIANT::from(i32::from(on))) }
    }
}

impl Mode {
    pub(super) fn attach(
        manager: &ITfThreadMgr,
        tid: u32,
        sink: &ITfCompartmentEventSink,
    ) -> Result<Self> {
        let compartments = manager.cast::<ITfCompartmentMgr>()?;
        let mut mode = Self {
            switch: Switch {
                open: unsafe { compartments.GetCompartment(&GUID_COMPARTMENT_KEYBOARD_OPENCLOSE)? },
                conversion: unsafe {
                    compartments.GetCompartment(&GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION)?
                },
            },
            changes: Vec::new(),
            keys: manager.cast()?,
            registered: Vec::new(),
        };
        for (slot, default) in [
            (&mode.switch.conversion, JAPANESE as i32),
            (&mode.switch.open, 1),
        ] {
            if value(slot).is_none() {
                unsafe { slot.SetValue(tid, &VARIANT::from(default))? };
            }
            let source = slot.cast::<ITfSource>()?;
            let cookie = unsafe { source.AdviseSink(&ITfCompartmentEventSink::IID, sink)? };
            mode.changes.push((source, cookie));
        }
        let name: Vec<_> = "Kanade 入力切替".encode_utf16().collect();
        for binding in KEYS {
            if unsafe { mode.keys.PreserveKey(tid, &binding.id, &binding.key, &name) }.is_ok() {
                mode.registered.push(binding);
            }
        }
        Ok(mode)
    }

    pub(super) fn switch(&self) -> Switch {
        self.switch.clone()
    }

    pub(super) fn japanese(&self) -> bool {
        self.switch.japanese()
    }

    pub(super) fn apply(&self, tid: u32, command: Command) -> Result<()> {
        self.switch.apply(tid, command)
    }
}

impl Drop for Mode {
    fn drop(&mut self) {
        for (source, cookie) in self.changes.drain(..) {
            let _ = unsafe { source.UnadviseSink(cookie) };
        }
        for b in self.registered.drain(..) {
            let _ = unsafe { self.keys.UnpreserveKey(&b.id, &b.key) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // An isolated STA thread exercises the real Windows TSF compartments.
    // The null document also proves that direct keys bypass scope, IPC and edits.
    #[test]
    fn windows_tsf_switches_between_kana_and_direct_input() -> Result<()> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
        let result = (|| unsafe {
            let thread: ITfThreadMgr =
                CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
            let tid = thread.Activate()?;
            let tip = windows_core::ComObject::new(Tip::new());
            let keys: ITfKeyEventSink = tip.to_interface();
            tip.shared.tid.set(tid);
            let mode = Mode::attach(&thread, tid, &tip.to_interface())?;
            tip.shared.japanese.set(mode.japanese());
            *tip.mode.borrow_mut() = Some(mode);
            let slots = thread.cast::<ITfCompartmentMgr>()?;
            let open = slots.GetCompartment(&GUID_COMPARTMENT_KEYBOARD_OPENCLOSE)?;
            let conversion =
                slots.GetCompartment(&GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION)?;
            let saved_open = open.GetValue()?;
            let saved_conversion = conversion.GetValue()?;
            let check = (|| -> Result<()> {
                for key in [0x16usize, 0x19] {
                    assert!(keys.OnTestKeyDown(None, WPARAM(key), LPARAM(0))?.as_bool());
                    assert!(keys.OnKeyDown(None, WPARAM(key), LPARAM(0))?.as_bool());
                    assert!(keys.OnKeyUp(None, WPARAM(key), LPARAM(0))?.as_bool());
                }
                assert_eq!(value(&open), Some(0));
                for key in [0x41usize, 0x31, 0xba, 0x20, 0x0d, 0x08] {
                    assert!(!keys.OnTestKeyDown(None, WPARAM(key), LPARAM(0))?.as_bool());
                    assert!(!keys.OnKeyDown(None, WPARAM(key), LPARAM(0))?.as_bool());
                    assert!(!keys.OnKeyUp(None, WPARAM(key), LPARAM(0))?.as_bool());
                }
                let toggle = KEYS
                    .iter()
                    .find(|b| b.key.uVKey == 0xf3)
                    .ok_or_else(failure)?;
                assert!(keys.OnPreservedKey(None, &toggle.id)?.as_bool());
                assert_eq!(value(&open), Some(1));
                // In Japanese mode a regular key needs a document context.
                assert!(keys.OnTestKeyDown(None, WPARAM(0x41), LPARAM(0)).is_err());
                conversion.SetValue(tid, &VARIANT::from(0i32))?;
                assert!(!keys.OnTestKeyDown(None, WPARAM(0x41), LPARAM(0))?.as_bool());
                assert!(keys.OnPreservedKey(None, &toggle.id)?.as_bool());
                assert_ne!(
                    value(&conversion).ok_or_else(failure)? as u32 & TF_CONVERSIONMODE_NATIVE,
                    0
                );
                Ok(())
            })();
            open.SetValue(tid, &saved_open)?;
            conversion.SetValue(tid, &saved_conversion)?;
            let mode = tip.mode.borrow_mut().take();
            drop(mode);
            thread.Deactivate()?;
            check
        })();
        unsafe { CoUninitialize() };
        result
    }
}
