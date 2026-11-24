//! A synthetic candidate fixture for visual checks, without an input host or IPC.
#[cfg(target_os = "macos")]
#[path = "../src/native/candidates.rs"]
mod candidates;

#[cfg(target_os = "macos")]
fn main() {
    use kanade_ipc::{CandidateItem, CandidatePage};
    use objc2::{MainThreadMarker, MainThreadOnly, rc::autoreleasepool, sel};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSMenu, NSMenuItem,
        NSPanel, NSWindowStyleMask,
    };
    use objc2_foundation::{NSPoint, NSRect, NSString};
    autoreleasepool(|_| {
        let mtm = MainThreadMarker::new().expect("main thread");
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        let menu = NSMenu::new(mtm);
        let app_item = NSMenuItem::new(mtm);
        let app_menu = NSMenu::new(mtm);
        let quit = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("終了"),
                Some(sel!(terminate:)),
                &NSString::from_str("q"),
            )
        };
        app_menu.addItem(&quit);
        app_item.setSubmenu(Some(&app_menu));
        menu.addItem(&app_item);
        app.setMainMenu(Some(&menu));
        let large = std::env::args().any(|arg| arg == "--large")
            || std::env::var_os("KANADE_PREVIEW_LARGE").is_some();
        let page = CandidatePage {
            items: [
                ("日本語", "文脈"),
                ("にほんご", "ひらがな"),
                ("ニホンゴ", "カタカナ"),
                ("nihongo", "アルファベット"),
                ("入力", ""),
                ("学習語", "学習した語"),
                ("ProjectName", "プロジェクトの語"),
                (
                    "長い変換候補が表示領域を超える場合の省略表示も確認します",
                    "",
                ),
                ("候補", ""),
            ]
            .into_iter()
            .enumerate()
            .map(|(i, (value, annotation))| CandidateItem {
                value: value.into(),
                reading: "このよみはひょうじしない".into(),
                annotation: annotation.into(),
                ai_recommended: i == 0,
                number: i as u32 + 1,
                ..Default::default()
            })
            .collect(),
            total: 14,
            font_size: if large { 40.0 } else { 16.0 },
            footer: "このふったーもひょうじしない".into(),
            ..Default::default()
        };
        let (view, size) = candidates::content(&page, None, mtm);
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            NSRect::new(NSPoint::new(400.0, 300.0), size),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        );
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setTitle(&NSString::from_str("Kanade 候補表示の確認"));
        panel.setContentView(Some(&view));
        panel.makeKeyAndOrderFront(None);
        app.run();
    });
}

#[cfg(not(target_os = "macos"))]
fn main() {}
