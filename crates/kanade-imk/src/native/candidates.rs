use kanade_ipc::CandidatePage;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, rc::Retained, runtime::AnyObject, sel};
use objc2_app_kit::{
    NSBox, NSBoxType, NSButton, NSColor, NSFont, NSFontAttributeName,
    NSForegroundColorAttributeName, NSLineBreakMode, NSTextAlignment, NSTextField, NSView,
};
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSMutableAttributedString, NSPoint, NSRect, NSSize, NSString,
};

fn text(value: &str, font: &NSFont, color: &NSColor) -> Retained<NSAttributedString> {
    let attributes = NSDictionary::from_slices(
        &[unsafe { NSFontAttributeName }, unsafe {
            NSForegroundColorAttributeName
        }],
        &[font as &AnyObject, color as &AnyObject],
    );
    // The dictionary contains the documented NSFont and NSColor attribute values.
    unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(value),
            Some(&attributes),
        )
    }
}

pub(super) fn content(
    page: &CandidatePage,
    target: Option<&AnyObject>,
    mtm: MainThreadMarker,
) -> (Retained<NSView>, NSSize) {
    let font_size = f64::from(page.font_size.clamp(12.0, 40.0));
    let font = NSFont::systemFontOfSize(font_size);
    let small = NSFont::systemFontOfSize((font_size * 0.78).max(11.0));
    let row_height = font_size + 12.0;
    let footer_height = 25.0;
    let height = page.items.len() as f64 * row_height + footer_height + 8.0;
    let mut width = 200.0_f64;
    let mut buttons = Vec::new();
    for (row, item) in page.items.iter().enumerate() {
        let selected = page.start + row as u32 == page.selected;
        let color = if selected {
            NSColor::whiteColor()
        } else {
            NSColor::labelColor()
        };
        let secondary = if selected {
            color.clone()
        } else {
            NSColor::secondaryLabelColor()
        };
        let title = NSMutableAttributedString::initWithAttributedString(
            NSMutableAttributedString::alloc(),
            &text(&format!("{}   ", item.number), &small, &secondary),
        );
        title.appendAttributedString(&text(&item.value, &font, &color));
        if !item.annotation.is_empty()
            && !matches!(
                item.annotation.as_str(),
                "ひらがな" | "カタカナ" | "アルファベット"
            )
        {
            title.appendAttributedString(&text(
                &format!("   {}", item.annotation),
                &small,
                &secondary,
            ));
        }
        let button = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::new(),
                target,
                target.map(|_| sel!(chooseCandidate:)),
                mtm,
            )
        };
        button.setAttributedTitle(&title);
        button.setTag(row as isize);
        button.setBordered(false);
        button.setAlignment(NSTextAlignment::Left);
        button.setUsesSingleLineMode(true);
        button.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        width = width.max(button.fittingSize().width + 32.0);
        buttons.push((button, selected));
    }
    width = width.ceil().min(480.0);
    let size = NSSize::new(width, height);
    let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::ZERO, size));
    for (row, (button, selected)) in buttons.into_iter().enumerate() {
        let y = height - 4.0 - row_height * (row as f64 + 1.0);
        if selected {
            let highlight = NSBox::initWithFrame(
                NSBox::alloc(mtm),
                NSRect::new(NSPoint::new(4.0, y), NSSize::new(width - 8.0, row_height)),
            );
            highlight.setBoxType(NSBoxType::Custom);
            highlight.setBorderWidth(0.0);
            highlight.setCornerRadius(4.0);
            highlight.setFillColor(&NSColor::selectedContentBackgroundColor());
            view.addSubview(&highlight);
        }
        button.setFrame(NSRect::new(
            NSPoint::new(12.0, y),
            NSSize::new(width - 24.0, row_height),
        ));
        view.addSubview(&button);
    }
    let footer = NSTextField::labelWithString(
        &NSString::from_str(&format!("{} / {}", page.selected + 1, page.total)),
        mtm,
    );
    footer.setTextColor(Some(&NSColor::secondaryLabelColor()));
    footer.setFont(Some(&NSFont::systemFontOfSize(11.0)));
    footer.setFrame(NSRect::new(
        NSPoint::new(12.0, 5.0),
        NSSize::new(width - 24.0, footer_height - 5.0),
    ));
    view.addSubview(&footer);
    (view, size)
}
