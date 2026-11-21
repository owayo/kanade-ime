//! Draws the あ / A icon of the taskbar input indicator.
//!
//! The indicator shows the icon as it is. The glyph is therefore drawn in the
//! colour of the taskbar text and outlined in the opposite colour, so that it
//! stays legible when the taskbar theme changes after the icon was made.
use super::*;
use windows::{
    Win32::{
        Graphics::Gdi::*,
        System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
        UI::{
            Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW},
            HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext},
            WindowsAndMessaging::*,
        },
    },
    core::w,
};

/// Colours of the glyph and of its outline, as red, green and blue.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Palette {
    pub glyph: [u8; 3],
    pub outline: [u8; 3],
}

fn channels(color: u32) -> [u8; 3] {
    // COLORREF is 0x00bbggrr.
    [color as u8, (color >> 8) as u8, (color >> 16) as u8]
}

impl Palette {
    /// The Windows guideline for mode icons: a white glyph, a dark outline.
    pub(super) const ON_DARK: Self = Self {
        glyph: [255; 3],
        outline: [0; 3],
    };
    pub(super) const ON_LIGHT: Self = Self {
        glyph: [0; 3],
        outline: [255; 3],
    };

    /// `contrast` is the text and background colour of an active contrast
    /// theme. `light` is the taskbar setting, when it can be read.
    pub(super) fn choose(contrast: Option<(u32, u32)>, light: Option<bool>) -> Self {
        match (contrast, light) {
            (Some((text, background)), _) => Self {
                glyph: channels(text),
                outline: channels(background),
            },
            (None, Some(true)) => Self::ON_LIGHT,
            // A dark taskbar, and Windows versions whose taskbar is always dark.
            (None, _) => Self::ON_DARK,
        }
    }

    pub(super) fn of_taskbar() -> Self {
        Self::choose(contrast_theme(), light_taskbar())
    }
}

fn contrast_theme() -> Option<(u32, u32)> {
    let mut state = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            state.cbSize,
            Some((&raw mut state).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .ok()?;
        (state.dwFlags.0 & HCF_HIGHCONTRASTON.0 != 0)
            .then(|| (GetSysColor(COLOR_WINDOWTEXT), GetSysColor(COLOR_WINDOW)))
    }
}

fn light_taskbar() -> Option<bool> {
    let mut value = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    };
    (status == ERROR_SUCCESS).then_some(value != 0)
}

/// The small-icon size of the display, in real pixels. A host that is not
/// DPI aware would otherwise report 16 on every display and the indicator
/// would enlarge the icon.
pub(super) fn small_icon_size() -> i32 {
    unsafe {
        let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let size = GetSystemMetrics(SM_CXSMICON);
        if !previous.0.is_null() {
            SetThreadDpiAwarenessContext(previous);
        }
        size
    }
}

/// Glyph coverage cropped to its ink, one byte for each pixel.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Mask {
    width: usize,
    height: usize,
    coverage: Vec<u8>,
}

impl Mask {
    /// Crops rows of `width` pixels to the ink, ignoring smoothing residue.
    pub(super) fn crop(width: usize, coverage: &[u8]) -> Option<Self> {
        const INK: u8 = 24;
        if width == 0 {
            return None;
        }
        let inked = || {
            coverage
                .iter()
                .enumerate()
                .filter(|(_, level)| **level >= INK)
                .map(|(index, _)| (index % width, index / width))
        };
        let left = inked().map(|(x, _)| x).min()?;
        let right = inked().map(|(x, _)| x).max()?;
        let top = inked().map(|(_, y)| y).min()?;
        let bottom = inked().map(|(_, y)| y).max()?;
        let cropped = coverage
            .chunks(width)
            .skip(top)
            .take(bottom - top + 1)
            .flat_map(|row| row.iter().skip(left).take(right - left + 1).copied())
            .collect();
        Some(Self {
            width: right - left + 1,
            height: bottom - top + 1,
            coverage: cropped,
        })
    }

    fn at(&self, x: isize, y: isize) -> u32 {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
            return 0;
        }
        self.coverage
            .get(y as usize * self.width + x as usize)
            .map_or(0, |level| u32::from(*level))
    }
}

/// Centres the glyph in a square of `size` pixels. The result is blue, green,
/// red and straight (not premultiplied) alpha for each pixel.
pub(super) fn compose(mask: &Mask, size: usize, palette: Palette) -> Vec<u8> {
    // The outline is one pixel wide at half strength, weaker on the diagonals.
    const AROUND: [(isize, isize, u32); 8] = [
        (-1, 0, 128),
        (1, 0, 128),
        (0, -1, 128),
        (0, 1, 128),
        (-1, -1, 80),
        (1, -1, 80),
        (-1, 1, 80),
        (1, 1, 80),
    ];
    let left = (size as isize - mask.width as isize) / 2;
    let top = (size as isize - mask.height as isize) / 2;
    let mut pixels = Vec::with_capacity(size * size * 4);
    for y in 0..size as isize {
        for x in 0..size as isize {
            let (gx, gy) = (x - left, y - top);
            let glyph = mask.at(gx, gy);
            let around = AROUND
                .iter()
                .map(|(dx, dy, strength)| mask.at(gx + dx, gy + dy) * strength / 255)
                .max()
                .unwrap_or(0);
            let outline = around * (255 - glyph) / 255;
            let alpha = glyph + outline;
            let [red, green, blue] = [0, 1, 2].map(|channel| {
                let glyph_color = palette.glyph.get(channel).map_or(0, |c| u32::from(*c));
                let outline_color = palette.outline.get(channel).map_or(0, |c| u32::from(*c));
                (glyph_color * glyph + outline_color * outline + alpha / 2)
                    .checked_div(alpha)
                    .unwrap_or(0) as u8
            });
            pixels.extend([blue, green, red, alpha as u8]);
        }
    }
    pixels
}

struct Dc(HDC);
impl Drop for Dc {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.0);
        }
    }
}
struct Object(HGDIOBJ);
impl Drop for Object {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.0);
        }
    }
}

/// A top-down 32-bit bitmap of `side` pixels square, with its pixel memory.
fn bitmap(dc: Option<HDC>, side: i32) -> Result<(Object, *mut u8)> {
    let header = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: side,
            biHeight: -side,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let bitmap = Object(
        unsafe { CreateDIBSection(dc, &header, DIB_RGB_COLORS, &mut bits, None, 0)? }.into(),
    );
    if bits.is_null() {
        return Err(failure());
    }
    Ok((bitmap, bits.cast()))
}

/// Draws `text` white on black with grey-scale smoothing and keeps its ink.
fn rasterize(text: &str, em: i32) -> Result<Mask> {
    // Room for one glyph of this size wherever the font places it.
    let side = em.max(4) * 3;
    unsafe {
        let dc = Dc(CreateCompatibleDC(None));
        if dc.0.is_invalid() {
            return Err(failure());
        }
        let (canvas, bits) = bitmap(Some(dc.0), side)?;
        let font = Object(
            CreateFontW(
                -em,
                0,
                0,
                0,
                FW_SEMIBOLD.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_TT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                w!("Yu Gothic UI"),
            )
            .into(),
        );
        if font.0.is_invalid() {
            return Err(failure());
        }
        let previous_bitmap = SelectObject(dc.0, canvas.0);
        let previous_font = SelectObject(dc.0, font.0);
        SetTextColor(dc.0, COLORREF(0x00ff_ffff));
        SetBkMode(dc.0, TRANSPARENT);
        let units: Vec<u16> = text.encode_utf16().collect();
        let drawn = TextOutW(dc.0, em, em, &units).as_bool();
        let _ = GdiFlush();
        // Grey-scale smoothing writes the same level to every colour channel.
        let coverage: Vec<u8> = std::slice::from_raw_parts(bits, (side * side * 4) as usize)
            .as_chunks::<4>()
            .0
            .iter()
            .map(|[_, green, _, _]| *green)
            .collect();
        SelectObject(dc.0, previous_font);
        SelectObject(dc.0, previous_bitmap);
        if !drawn {
            return Err(failure());
        }
        Mask::crop(side as usize, &coverage).ok_or_else(failure)
    }
}

/// The font size at which あ fills about four fifths of the icon. Every mode
/// uses it, so the glyphs keep their natural proportion to each other.
fn em_for(size: i32) -> Result<i32> {
    let probe = rasterize("あ", size)?;
    let target = (size * 4 + 2) / 5;
    Ok((size * target / (probe.height as i32).max(1)).clamp(6, size * 2))
}

/// Makes an icon of `size` pixels from straight-alpha pixels.
fn icon(size: i32, pixels: &[u8]) -> Result<HICON> {
    let side = size as usize;
    if pixels.len() != side * side * 4 {
        return Err(failure());
    }
    unsafe {
        let (color, bits) = bitmap(None, size)?;
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits, pixels.len());
        // The alpha channel decides the transparency. The mask repeats it for
        // a caller that draws without alpha. Its rows are 16-bit aligned.
        let stride = side.div_ceil(16) * 2;
        let mut transparent = vec![0u8; stride * side];
        for (index, [_, _, _, alpha]) in pixels.as_chunks::<4>().0.iter().enumerate() {
            if *alpha == 0
                && let Some(byte) = transparent.get_mut(index / side * stride + index % side / 8)
            {
                *byte |= 0x80 >> (index % side % 8);
            }
        }
        let mask = Object(CreateBitmap(size, size, 1, 1, Some(transparent.as_ptr().cast())).into());
        if mask.0.is_invalid() {
            return Err(failure());
        }
        CreateIconIndirect(&ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: HBITMAP(mask.0.0),
            hbmColor: HBITMAP(color.0.0),
        })
    }
}

/// Draws `glyph` as an icon of `size` pixels. The caller destroys the icon.
pub(super) fn render(glyph: &str, size: i32, palette: Palette) -> Result<HICON> {
    let size = size.clamp(16, 256);
    let mask = rasterize(glyph, em_for(size)?)?;
    icon(size, &compose(&mask, size as usize, palette))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Icon(HICON);
    impl Drop for Icon {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyIcon(self.0);
            }
        }
    }

    /// The icon drawn over `background`: red, green and blue of each pixel.
    fn drawn(icon: &Icon, size: i32, background: u8) -> Result<Vec<[u8; 3]>> {
        unsafe {
            let dc = Dc(CreateCompatibleDC(None));
            let (canvas, bits) = bitmap(Some(dc.0), size)?;
            let pixels = std::slice::from_raw_parts_mut(bits, (size * size * 4) as usize);
            pixels.fill(background);
            let previous = SelectObject(dc.0, canvas.0);
            let result = DrawIconEx(dc.0, 0, 0, icon.0, size, size, 0, None, DI_NORMAL);
            let _ = GdiFlush();
            SelectObject(dc.0, previous);
            result?;
            Ok(pixels
                .as_chunks::<4>()
                .0
                .iter()
                .map(|[blue, green, red, _]| [*red, *green, *blue])
                .collect())
        }
    }

    fn differing(pixels: &[[u8; 3]], background: u8, by: u8) -> usize {
        pixels
            .iter()
            .filter(|pixel| pixel.iter().all(|c| c.abs_diff(background) >= by))
            .count()
    }

    #[test]
    fn colours_follow_the_taskbar_and_a_contrast_theme() {
        assert_eq!(Palette::choose(None, Some(false)), Palette::ON_DARK);
        assert_eq!(Palette::choose(None, Some(true)), Palette::ON_LIGHT);
        // Windows without the setting has a dark taskbar.
        assert_eq!(Palette::choose(None, None), Palette::ON_DARK);
        // Yellow text on dark blue wins over the taskbar setting.
        assert_eq!(
            Palette::choose(Some((0x0000_ffff, 0x0080_0000)), Some(true)),
            Palette {
                glyph: [255, 255, 0],
                outline: [0, 0, 128],
            }
        );
        // The real desktop always yields one of the palettes.
        let _ = Palette::of_taskbar();
    }

    #[test]
    fn the_glyph_is_centred_and_outlined() -> Result<()> {
        // A block of ink two pixels square, and residue below the ink level.
        let mut coverage = vec![0u8; 36];
        for index in [8, 9, 14, 15] {
            *coverage.get_mut(index).ok_or_else(failure)? = 255;
        }
        *coverage.get_mut(35).ok_or_else(failure)? = 10;
        let mask = Mask::crop(6, &coverage).ok_or_else(failure)?;
        assert_eq!((mask.width, mask.height), (2, 2));
        let pixels = compose(&mask, 6, Palette::ON_DARK);
        let pixel = |x: usize, y: usize| pixels.as_chunks::<4>().0.get(y * 6 + x);
        // The glyph is opaque white in the middle.
        for (x, y) in [(2, 2), (3, 2), (2, 3), (3, 3)] {
            assert_eq!(pixel(x, y), Some(&[255, 255, 255, 255]));
        }
        // One pixel of black at half strength beside it, less on a diagonal.
        assert_eq!(pixel(1, 2), Some(&[0, 0, 0, 128]));
        assert_eq!(pixel(4, 3), Some(&[0, 0, 0, 128]));
        assert_eq!(pixel(1, 1), Some(&[0, 0, 0, 80]));
        // Nothing two pixels away.
        assert_eq!(pixel(0, 2), Some(&[0, 0, 0, 0]));
        assert_eq!(pixels.len(), 6 * 6 * 4);
        // A glyph larger than the icon is clipped, not an error.
        assert_eq!(compose(&mask, 1, Palette::ON_LIGHT), [0, 0, 0, 255]);
        Ok(())
    }

    #[test]
    fn a_drawing_without_ink_has_no_glyph() {
        assert_eq!(Mask::crop(4, &[0; 16]), None);
        assert_eq!(Mask::crop(4, &[10; 16]), None);
        assert_eq!(Mask::crop(0, &[]), None);
    }

    #[test]
    fn partly_transparent_pixels_blend_with_the_taskbar() -> Result<()> {
        // White at half strength, opaque white, then transparent pixels.
        let mut pixels = vec![0u8; 16 * 16 * 4];
        for (slot, value) in pixels
            .iter_mut()
            .zip([255, 255, 255, 128, 255, 255, 255, 255])
        {
            *slot = value;
        }
        let icon = Icon(icon(16, &pixels)?);
        let on_black = drawn(&icon, 16, 0)?;
        let half = on_black.first().ok_or_else(failure)?;
        assert!(half.iter().all(|c| (116..=140).contains(c)), "{half:?}");
        assert_eq!(on_black.get(1), Some(&[255; 3]));
        assert_eq!(on_black.get(2), Some(&[0; 3]));
        let on_white = drawn(&icon, 16, 255)?;
        assert_eq!(on_white.first(), Some(&[255; 3]));
        assert_eq!(on_white.get(2), Some(&[255; 3]));
        Ok(())
    }

    #[test]
    fn icons_show_each_mode_on_light_and_dark_taskbars() -> Result<()> {
        const DARK: u8 = 32;
        const LIGHT: u8 = 243;
        for size in [16, 20, 24, 32, 48] {
            let area = (size * size) as usize;
            let mut drawings = Vec::new();
            for glyph in ["あ", "A"] {
                for (palette, taskbar, other) in [
                    (Palette::ON_DARK, DARK, LIGHT),
                    (Palette::ON_LIGHT, LIGHT, DARK),
                ] {
                    let icon = Icon(render(glyph, size, palette)?);
                    // The indicator needs a colour bitmap of the asked size.
                    let mut info = ICONINFO::default();
                    unsafe { GetIconInfo(icon.0, &mut info)? };
                    let (color, mask) = (Object(info.hbmColor.into()), Object(info.hbmMask.into()));
                    assert!(!color.0.is_invalid() && !mask.0.is_invalid());
                    let mut shape = BITMAP::default();
                    let read = unsafe {
                        GetObjectW(
                            color.0,
                            std::mem::size_of::<BITMAP>() as i32,
                            Some((&raw mut shape).cast()),
                        )
                    };
                    assert!(read != 0);
                    assert_eq!((shape.bmWidth, shape.bmHeight), (size, size));
                    // A glyph, not a filled block, in strong contrast.
                    let pixels = drawn(&icon, size, taskbar)?;
                    let strong = differing(&pixels, taskbar, 150);
                    assert!(strong >= area / 16, "{glyph} {size}: {strong}");
                    assert!(differing(&pixels, taskbar, 30) <= area * 3 / 5);
                    // The outline keeps it legible on the other theme.
                    let stale = drawn(&icon, size, other)?;
                    assert!(differing(&stale, other, 60) >= size as usize);
                    // The outline leaves the edge of the icon free.
                    let edge = pixels.iter().take(size as usize);
                    assert_eq!(
                        differing(&edge.copied().collect::<Vec<_>>(), taskbar, 30),
                        0
                    );
                    drawings.push(pixels);
                }
            }
            // The two modes and the two themes are four different pictures.
            for (index, drawing) in drawings.iter().enumerate() {
                assert!(
                    drawings
                        .iter()
                        .skip(index + 1)
                        .all(|other| other != drawing)
                );
            }
        }
        Ok(())
    }

    #[test]
    fn the_icon_size_is_that_of_a_small_icon() {
        assert!((16..=256).contains(&small_icon_size()));
    }

    /// Writes every icon over both taskbar colours as a 24-bit bitmap, for a
    /// person to look at: `KANADE_ICON_SHEET` names the file.
    #[test]
    #[ignore = "writes an image for visual review"]
    fn icon_sheet() -> Result<()> {
        let Some(path) = std::env::var_os("KANADE_ICON_SHEET") else {
            return Ok(());
        };
        const SIZES: [i32; 5] = [16, 20, 24, 32, 48];
        const CELL: usize = 56;
        let cells: Vec<(&str, Palette, u8)> = ["あ", "A"]
            .into_iter()
            .flat_map(|glyph| {
                [
                    (glyph, Palette::ON_DARK, 32),
                    (glyph, Palette::ON_LIGHT, 243),
                    (glyph, Palette::ON_DARK, 243),
                    (glyph, Palette::ON_LIGHT, 32),
                ]
            })
            .collect();
        let (width, height) = (cells.len() * CELL, SIZES.len() * CELL);
        let mut sheet = vec![0u8; width * height * 3];
        for (row, size) in SIZES.into_iter().enumerate() {
            for (column, (glyph, palette, background)) in cells.iter().enumerate() {
                let icon = Icon(render(glyph, size, *palette)?);
                let pixels = drawn(&icon, size, *background)?;
                for y in 0..CELL {
                    for x in 0..CELL {
                        let inside = x.checked_sub(4).zip(y.checked_sub(4)).and_then(|(x, y)| {
                            (x < size as usize && y < size as usize)
                                .then(|| pixels.get(y * size as usize + x))
                                .flatten()
                        });
                        let [red, green, blue] = inside.copied().unwrap_or([*background; 3]);
                        // Bitmap rows run from the bottom, pixels as blue, green, red.
                        let offset =
                            ((height - 1 - (row * CELL + y)) * width + column * CELL + x) * 3;
                        for (slot, value) in sheet.iter_mut().skip(offset).zip([blue, green, red]) {
                            *slot = value;
                        }
                    }
                }
            }
        }
        let mut file = Vec::with_capacity(54 + sheet.len());
        file.extend(b"BM");
        file.extend((54 + sheet.len() as u32).to_le_bytes());
        file.extend([0u8; 4]);
        file.extend(54u32.to_le_bytes());
        file.extend(40u32.to_le_bytes());
        file.extend((width as i32).to_le_bytes());
        file.extend((height as i32).to_le_bytes());
        file.extend(1u16.to_le_bytes());
        file.extend(24u16.to_le_bytes());
        file.extend([0u8; 24]);
        file.extend(sheet);
        std::fs::write(path, file).map_err(|_| failure())
    }
}
