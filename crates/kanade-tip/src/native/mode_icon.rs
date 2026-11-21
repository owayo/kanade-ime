//! Draws the あ / A icon of the taskbar input indicator.
//!
//! The indicator shows the icon as it is, beside text and icons that the
//! taskbar draws itself: thin strokes in one colour. The glyph is drawn the
//! same way, in the regular weight and in the colour of the taskbar text,
//! with nothing around it. An outline in the opposite colour would keep the
//! glyph legible on the other theme, but it shows as a halo on the real
//! taskbar. The button reads the theme again instead.
use super::*;
use windows::{
    Win32::{
        Graphics::{
            Direct2D::{Common::*, *},
            DirectWrite::*,
            Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            Gdi::*,
        },
        System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
        UI::{
            Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW},
            HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext},
            WindowsAndMessaging::*,
        },
    },
    core::w,
};

/// Colour of the glyph as red, green and blue, and how opaque it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Palette {
    pub glyph: [u8; 3],
    pub opacity: u8,
}

fn channels(color: u32) -> [u8; 3] {
    // COLORREF is 0x00bbggrr.
    [color as u8, (color >> 8) as u8, (color >> 16) as u8]
}

impl Palette {
    /// The text of a dark taskbar is white.
    pub(super) const ON_DARK: Self = Self {
        glyph: [255; 3],
        opacity: 255,
    };
    /// The text of a light taskbar is black at nine tenths.
    pub(super) const ON_LIGHT: Self = Self {
        glyph: [0; 3],
        opacity: 228,
    };

    /// `contrast` is the text colour of an active contrast theme. `light` is
    /// the taskbar setting, when it can be read.
    pub(super) fn choose(contrast: Option<u32>, light: Option<bool>) -> Self {
        match (contrast, light) {
            (Some(text), _) => Self {
                glyph: channels(text),
                opacity: 255,
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

fn contrast_theme() -> Option<u32> {
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
        (state.dwFlags.0 & HCF_HIGHCONTRASTON.0 != 0).then(|| GetSysColor(COLOR_WINDOWTEXT))
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
    let left = (size as isize - mask.width as isize) / 2;
    let top = (size as isize - mask.height as isize) / 2;
    let [red, green, blue] = palette.glyph;
    let mut pixels = Vec::with_capacity(size * size * 4);
    for y in 0..size as isize {
        for x in 0..size as isize {
            let alpha = (mask.at(x - left, y - top) * u32::from(palette.opacity) + 127) / 255;
            // Only the glyph is drawn: a pixel without ink carries no colour.
            pixels.extend(if alpha == 0 {
                [0; 4]
            } else {
                [blue, green, red, alpha as u8]
            });
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

/// Which way the glyph contrasts with the taskbar. DirectWrite shapes the
/// edge of a stroke for the colours it blends, so light text and dark text
/// are drawn separately.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ink {
    Light,
    Dark,
}

impl Palette {
    fn ink(self) -> Ink {
        let [red, green, blue] = self.glyph.map(u32::from);
        if red * 299 + green * 587 + blue * 114 >= 128_000 {
            Ink::Light
        } else {
            Ink::Dark
        }
    }
}

/// A square memory bitmap that GDI and Direct2D draw into.
struct Canvas {
    dc: Dc,
    _bitmap: Object,
    previous: HGDIOBJ,
    bits: *mut u8,
    side: i32,
}

impl Canvas {
    fn new(side: i32) -> Result<Self> {
        unsafe {
            let dc = Dc(CreateCompatibleDC(None));
            if dc.0.is_invalid() {
                return Err(failure());
            }
            let (bitmap, bits) = bitmap(Some(dc.0), side)?;
            let previous = SelectObject(dc.0, bitmap.0);
            Ok(Self {
                dc,
                _bitmap: bitmap,
                previous,
                bits,
                side,
            })
        }
    }

    /// The green level of every pixel. A grey drawing has the same level in
    /// each colour channel.
    fn levels(&self) -> Vec<u8> {
        unsafe {
            let _ = GdiFlush();
            std::slice::from_raw_parts(self.bits, (self.side * self.side * 4) as usize)
                .as_chunks::<4>()
                .0
                .iter()
                .map(|[_, green, _, _]| *green)
                .collect()
        }
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc.0, self.previous);
        }
    }
}

/// Room for one glyph of this size wherever the font places it.
fn canvas_side(em: i32) -> i32 {
    em.max(4) * 3
}

/// Draws glyphs as the taskbar draws its own text: with DirectWrite, in the
/// regular weight and with grey-scale smoothing. The regular weight has the
/// thin strokes of the text beside the icon; a heavier one closes the
/// counters of あ at this size. Starting Direct2D is the slow part of an
/// icon, so one pen measures and draws it.
struct Pen {
    factory: ID2D1Factory,
    fonts: IDWriteFactory,
}

impl Pen {
    fn new() -> Result<Self> {
        unsafe {
            Ok(Self {
                factory: D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?,
                fonts: DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?,
            })
        }
    }

    /// How much of each pixel the glyph of `text` covers.
    fn draw(&self, text: &str, em: i32, ink: Ink) -> Result<Mask> {
        let side = canvas_side(em);
        let canvas = Canvas::new(side)?;
        let (paper, pen) = match ink {
            Ink::Light => (0.0, 1.0),
            Ink::Dark => (1.0, 0.0),
        };
        let grey = |level: f32| D2D1_COLOR_F {
            r: level,
            g: level,
            b: level,
            a: 1.0,
        };
        self.paint(&canvas, text, em, &grey(paper), &grey(pen))?;
        let coverage: Vec<u8> = canvas
            .levels()
            .into_iter()
            .map(|level| match ink {
                Ink::Light => level,
                Ink::Dark => 255 - level,
            })
            .collect();
        Mask::crop(side as usize, &coverage).ok_or_else(failure)
    }

    fn paint(
        &self,
        canvas: &Canvas,
        text: &str,
        em: i32,
        paper: &D2D1_COLOR_F,
        pen: &D2D1_COLOR_F,
    ) -> Result<()> {
        let side = canvas.side;
        unsafe {
            let format = self.fonts.CreateTextFormat(
                w!("Yu Gothic UI"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                em as f32,
                w!("ja-JP"),
            )?;
            format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            // Drawn in memory: no window and no graphics device of the host.
            let target = self
                .factory
                .CreateDCRenderTarget(&D2D1_RENDER_TARGET_PROPERTIES {
                    r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_IGNORE,
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    ..Default::default()
                })?;
            let area = RECT {
                left: 0,
                top: 0,
                right: side,
                bottom: side,
            };
            target.BindDC(canvas.dc.0, &area)?;
            let brush = target.CreateSolidColorBrush(pen, None)?;
            target.BeginDraw();
            target.Clear(Some(paper));
            target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let units: Vec<u16> = text.encode_utf16().collect();
            target.DrawText(
                &units,
                &format,
                &D2D_RECT_F {
                    left: em as f32,
                    top: em as f32 / 2.0,
                    right: side as f32,
                    bottom: side as f32,
                },
                &brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
            target.EndDraw(None, None)
        }
    }
}

/// Draws `text` with GDI, white on black. A host in which Direct2D cannot
/// draw still gets its icon, with harder edges.
fn plain(text: &str, em: i32) -> Result<Mask> {
    let side = canvas_side(em);
    let canvas = Canvas::new(side)?;
    unsafe {
        let font = Object(
            CreateFontW(
                -em,
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
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
        let previous = SelectObject(canvas.dc.0, font.0);
        SetTextColor(canvas.dc.0, COLORREF(0x00ff_ffff));
        SetBkMode(canvas.dc.0, TRANSPARENT);
        let units: Vec<u16> = text.encode_utf16().collect();
        let drawn = TextOutW(canvas.dc.0, em, em, &units).as_bool();
        let coverage = canvas.levels();
        SelectObject(canvas.dc.0, previous);
        if !drawn {
            return Err(failure());
        }
        Mask::crop(side as usize, &coverage).ok_or_else(failure)
    }
}

fn rasterize(pen: Option<&Pen>, text: &str, em: i32, ink: Ink) -> Result<Mask> {
    match pen.map(|pen| pen.draw(text, em, ink)) {
        Some(Ok(mask)) => Ok(mask),
        _ => plain(text, em),
    }
}

/// The font size at which あ fills about four fifths of the icon. Every mode
/// uses it, so the glyphs keep their natural proportion to each other.
fn em_for(pen: Option<&Pen>, size: i32, ink: Ink) -> Result<i32> {
    let probe = rasterize(pen, "あ", size, ink)?;
    let target = (size * 4 + 2) / 5;
    Ok((size * target / (probe.height as i32).max(1)).clamp(6, size * 2))
}

/// The picture of one mode: a square of `size` pixels, each blue, green, red
/// and straight alpha. Drawing it takes tens of milliseconds, making an icon
/// of it does not, so the button keeps the pictures it has drawn.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Drawing {
    size: i32,
    pixels: Vec<u8>,
}

impl Drawing {
    /// Draws `glyph` for an icon of `size` pixels.
    pub(super) fn new(glyph: &str, size: i32, palette: Palette) -> Result<Self> {
        let size = size.clamp(16, 256);
        let pen = Pen::new().ok();
        let ink = palette.ink();
        let em = em_for(pen.as_ref(), size, ink)?;
        let mask = rasterize(pen.as_ref(), glyph, em, ink)?;
        Ok(Self {
            size,
            pixels: compose(&mask, size as usize, palette),
        })
    }

    /// A new icon of the picture. The caller destroys it.
    pub(super) fn icon(&self) -> Result<HICON> {
        icon(self.size, &self.pixels)
    }
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
        // The yellow text of a contrast theme wins over the taskbar setting.
        assert_eq!(
            Palette::choose(Some(0x0000_ffff), Some(true)),
            Palette {
                glyph: [255, 255, 0],
                opacity: 255,
            }
        );
        // The real desktop always yields one of the palettes.
        let _ = Palette::of_taskbar();
    }

    #[test]
    fn the_glyph_is_centred_in_the_text_colour_with_nothing_around_it() -> Result<()> {
        // A block of ink two pixels square with one half-covered pixel, and
        // residue below the ink level.
        let mut coverage = vec![0u8; 36];
        for (index, level) in [(8, 255), (9, 255), (14, 255), (15, 128), (35, 10)] {
            *coverage.get_mut(index).ok_or_else(failure)? = level;
        }
        let mask = Mask::crop(6, &coverage).ok_or_else(failure)?;
        assert_eq!((mask.width, mask.height), (2, 2));
        let pixels = compose(&mask, 6, Palette::ON_DARK);
        let pixel = |x: usize, y: usize| pixels.as_chunks::<4>().0.get(y * 6 + x);
        // The glyph is white in the middle, as opaque as its ink.
        for (x, y) in [(2, 2), (3, 2), (2, 3)] {
            assert_eq!(pixel(x, y), Some(&[255, 255, 255, 255]));
        }
        assert_eq!(pixel(3, 3), Some(&[255, 255, 255, 128]));
        // Every other pixel is empty: no outline and no halo.
        let inked = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| **pixel != [0; 4])
            .count();
        assert_eq!(inked, 4);
        assert_eq!(pixels.len(), 6 * 6 * 4);
        // The text of a light taskbar is not fully black.
        let light = compose(&mask, 6, Palette::ON_LIGHT);
        let pixel = |x: usize, y: usize| light.as_chunks::<4>().0.get(y * 6 + x);
        assert_eq!(pixel(2, 2), Some(&[0, 0, 0, 228]));
        assert_eq!(pixel(3, 3), Some(&[0, 0, 0, 114]));
        // A glyph larger than the icon is clipped, not an error.
        assert_eq!(compose(&mask, 1, Palette::ON_DARK), [255; 4]);
        Ok(())
    }

    #[test]
    fn a_drawing_without_ink_has_no_glyph() {
        assert_eq!(Mask::crop(4, &[0; 16]), None);
        assert_eq!(Mask::crop(4, &[10; 16]), None);
        assert_eq!(Mask::crop(0, &[]), None);
    }

    #[test]
    fn the_glyph_is_smoothed_as_the_text_of_the_taskbar_is() -> Result<()> {
        let pen = Pen::new()?;
        let partly = |mask: &Mask| {
            let covered = |level: &&u8| (48..=208).contains(*level);
            mask.coverage.iter().filter(covered).count()
        };
        for ink in [Ink::Light, Ink::Dark] {
            // The slanted strokes of A have partly covered pixels along their
            // edges, where a drawing without smoothing has steps.
            let letter = pen.draw("A", 14, ink)?;
            assert!(partly(&letter) >= 8, "{ink:?}: {}", partly(&letter));
            assert!(letter.coverage.iter().any(|level| *level >= 160));
            assert!((8..=14).contains(&letter.height), "{}", letter.height);
            let kana = pen.draw("あ", 14, ink)?;
            assert!((10..=16).contains(&kana.height), "{}", kana.height);
            assert_ne!(kana, letter);
        }
        // Without Direct2D the glyph keeps its size and gets harder edges.
        let smoothed = rasterize(Some(&pen), "あ", 14, Ink::Light)?;
        let fallback = rasterize(None, "あ", 14, Ink::Light)?;
        assert_eq!(smoothed, pen.draw("あ", 14, Ink::Light)?);
        assert_eq!(fallback, plain("あ", 14)?);
        assert!(fallback.height.abs_diff(smoothed.height) <= 2);
        assert!(fallback.width.abs_diff(smoothed.width) <= 2);
        Ok(())
    }

    #[test]
    fn a_drawing_makes_as_many_icons_as_are_asked_for() -> Result<()> {
        let drawing = Drawing::new("あ", 16, Palette::ON_LIGHT)?;
        assert_eq!(drawing, Drawing::new("あ", 16, Palette::ON_LIGHT)?);
        assert_ne!(drawing, Drawing::new("A", 16, Palette::ON_LIGHT)?);
        assert_ne!(drawing, Drawing::new("あ", 16, Palette::ON_DARK)?);
        assert_ne!(drawing, Drawing::new("あ", 20, Palette::ON_LIGHT)?);
        // The indicator destroys each icon it gets, so each is its own.
        let (first, second) = (Icon(drawing.icon()?), Icon(drawing.icon()?));
        assert_ne!(first.0, second.0);
        assert_eq!(drawn(&first, 16, 243)?, drawn(&second, 16, 243)?);
        // Sizes outside those of a small icon are brought into range.
        assert_eq!(Drawing::new("あ", 4, Palette::ON_LIGHT)?, drawing);
        Ok(())
    }
    #[test]
    fn light_and_dark_text_are_drawn_for_their_own_contrast() {
        assert_eq!(Palette::ON_DARK.ink(), Ink::Light);
        assert_eq!(Palette::ON_LIGHT.ink(), Ink::Dark);
        // The yellow text of a contrast theme is light, dark blue is dark.
        let colour = |glyph| Palette {
            glyph,
            opacity: 255,
        };
        assert_eq!(colour([255, 255, 0]).ink(), Ink::Light);
        assert_eq!(colour([0, 0, 128]).ink(), Ink::Dark);
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
                for (palette, taskbar) in [(Palette::ON_DARK, DARK), (Palette::ON_LIGHT, LIGHT)] {
                    let icon = Icon(Drawing::new(glyph, size, palette)?.icon()?);
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
                    // Thin strokes in clear contrast, not a filled block.
                    let pixels = drawn(&icon, size, taskbar)?;
                    let strong = differing(&pixels, taskbar, 100);
                    let touched = differing(&pixels, taskbar, 30);

                    assert!(strong >= size as usize, "{glyph} {size}: {strong}");
                    assert!(touched <= area / 3, "{glyph} {size}: {touched}");
                    // Nothing in the opposite colour surrounds the glyph: every
                    // pixel lies between the taskbar and the text colour.
                    let halo = pixels
                        .iter()
                        .flatten()
                        .filter(|c| {
                            if taskbar == LIGHT {
                                **c > taskbar
                            } else {
                                **c < taskbar
                            }
                        })
                        .count();
                    assert_eq!(halo, 0, "{glyph} {size}");
                    // The glyph leaves the edge of the icon free.
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
                ]
            })
            .collect();
        let (width, height) = (cells.len() * CELL, SIZES.len() * CELL);
        let mut sheet = vec![0u8; width * height * 3];
        for (row, size) in SIZES.into_iter().enumerate() {
            for (column, (glyph, palette, background)) in cells.iter().enumerate() {
                let icon = Icon(Drawing::new(glyph, size, *palette)?.icon()?);
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
