//! Verify the input-mode button of the installed TIP through the thread's language bar.
#![cfg(windows)]
#![allow(non_snake_case)]
mod common;
use std::{cell::RefCell, rc::Rc};
use windows::{
    Win32::{
        Foundation::POINT,
        Graphics::Gdi::*,
        UI::{TextServices::*, WindowsAndMessaging::*},
    },
    core::{GUID, Interface, OutRef, PCWSTR, Result, implement, w},
};

const TIP: GUID = GUID::from_u128(0x6dafa84e_c56a_42be_983e_0d18f3dc5b21);

#[implement(ITfLangBarItemSink)]
struct Updates(Rc<RefCell<Vec<u32>>>);
impl ITfLangBarItemSink_Impl for Updates_Impl {
    fn OnUpdate(&self, flags: u32) -> Result<()> {
        self.0.borrow_mut().push(flags);
        Ok(())
    }
}

#[implement(ITfMenu)]
struct Lines(Rc<RefCell<Vec<(u32, String)>>>);
impl ITfMenu_Impl for Lines_Impl {
    fn AddMenuItem(
        &self,
        _: u32,
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
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text.0, len as usize) })
        };
        self.0.borrow_mut().push((flags, text));
        Ok(())
    }
}

/// The colour pixels of an icon, which is destroyed.
unsafe fn pixels(icon: HICON) -> Result<Vec<u8>> {
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(icon, &mut info)?;
        assert!(
            !info.hbmColor.is_invalid(),
            "the indicator needs a colour icon"
        );
        let mut shape = BITMAP::default();
        GetObjectW(
            info.hbmColor.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&raw mut shape).cast()),
        );
        assert_eq!(shape.bmWidth, shape.bmHeight);
        assert!(shape.bmWidth >= 16);
        let mut header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: shape.bmWidth,
                biHeight: -shape.bmHeight,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u8; (shape.bmWidth * shape.bmHeight * 4) as usize];
        let dc = GetDC(None);
        let lines = GetDIBits(
            dc,
            info.hbmColor,
            0,
            shape.bmHeight as u32,
            Some(pixels.as_mut_ptr().cast()),
            &mut header,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, dc);
        let _ = DeleteObject(info.hbmColor.into());
        let _ = DeleteObject(info.hbmMask.into());
        DestroyIcon(icon)?;
        assert_eq!(lines, shape.bmHeight);
        Ok(pixels)
    }
}

#[test]
#[ignore = "requires a registered Kanade Japanese input profile"]
fn installed_profile_shows_its_mode_in_the_language_bar() -> Result<()> {
    common::with_rich_edit(w!(""), |_, keys, context, open| unsafe {
        let invoke = |vk| -> Result<()> {
            let id = keys.GetPreservedKey(
                context,
                &TF_PRESERVEDKEY {
                    uVKey: vk,
                    uModifiers: TF_MOD_IGNORE_ALL_MODIFIER,
                },
            )?;
            assert!(keys.SimulatePreservedKey(context, &id)?.as_bool());
            common::dispatch_pending();
            Ok(())
        };
        let opened = || {
            open.GetValue()
                .map(|v| v.Anonymous.Anonymous.Anonymous.lVal)
        };
        // Windows 8 and later show only this item in the input indicator.
        let items = keys.cast::<ITfLangBarItemMgr>()?;
        let item = items.GetItem(&GUID_LBI_INPUTMODE)?;
        let mut info = TF_LANGBARITEMINFO::default();
        item.GetInfo(&mut info)?;
        assert_eq!(info.clsidService, TIP);
        assert_eq!(info.guidItem, GUID_LBI_INPUTMODE);
        assert_ne!(info.dwStyle & TF_LBI_STYLE_BTN_BUTTON, 0);
        let button = item.cast::<ITfLangBarItemButton>()?;
        let updates = Rc::new(RefCell::new(Vec::new()));
        let sink: ITfLangBarItemSink = Updates(updates.clone()).into();
        let mut cookie = 0;
        items.AdviseItemSink(&sink, &mut cookie, &GUID_LBI_INPUTMODE)?;

        invoke(0x16)?;
        assert_eq!(opened()?, 1);
        assert_eq!(button.GetText()?.to_string(), "ひらがな");
        let japanese = pixels(button.GetIcon()?)?;
        updates.borrow_mut().clear();

        // A left click switches to direct input, as the key does.
        button.OnClick(TF_LBI_CLK_LEFT, POINT::default(), std::ptr::null())?;
        common::dispatch_pending();
        assert_eq!(opened()?, 0);
        assert_eq!(button.GetText()?.to_string(), "半角英数");
        assert!(item.GetTooltipString()?.to_string().contains("半角英数"));
        let direct = pixels(button.GetIcon()?)?;
        assert_ne!(japanese, direct);
        assert_eq!(updates.borrow().len(), 1);
        assert_ne!(
            updates.borrow().first().copied().unwrap_or(0) & TF_LBI_ICON,
            0
        );

        // The key switches back and the button follows it.
        invoke(0x19)?;
        assert_eq!(opened()?, 1);
        assert_eq!(button.GetText()?.to_string(), "ひらがな");
        assert_eq!(pixels(button.GetIcon()?)?, japanese);
        assert_eq!(updates.borrow().len(), 2);

        // The two read-only status rows follow the commands.
        let lines = Rc::new(RefCell::new(Vec::new()));
        let menu: ITfMenu = Lines(lines.clone()).into();
        button.InitMenu(&menu)?;
        assert_eq!(
            &lines.borrow()[..4],
            [
                (TF_LBMENUF_RADIOCHECKED, "ひらがな".to_owned()),
                (0, "半角英数".to_owned()),
                (TF_LBMENUF_SEPARATOR, String::new()),
                (0, "Kanade の設定…".to_owned()),
            ]
        );
        let lines = lines.borrow();
        assert_eq!(lines.len(), 6);
        for ((flags, label), prefix) in lines[4..].iter().zip(["変換エンジンの動作：", "AI："])
        {
            assert_eq!(*flags, TF_LBMENUF_GRAYED);
            assert!(label.starts_with(prefix) && label.len() > prefix.len());
        }
        button.OnMenuSelect(1)?;
        common::dispatch_pending();
        assert_eq!(opened()?, 0);
        button.OnMenuSelect(0)?;
        common::dispatch_pending();
        assert_eq!(opened()?, 1);
        items.UnadviseItemSink(cookie)?;
        Ok(())
    })
}
