//! Prefilter the artwork at the display's physical resolution. The GPU's
//! bilinear minification alone skips fine detail when reducing a large image.
use image::{ImageBuffer, Rgba, Rgba32FImage, RgbaImage, imageops::FilterType};
use std::{cell::RefCell, collections::BTreeMap, sync::Arc, sync::LazyLock};

pub(super) const POINTS: f32 = 44.0;
static SOURCE: LazyLock<Rgba32FImage> = LazyLock::new(|| {
    linear_premultiplied(
        &image::load_from_memory(include_bytes!("../../../../assets/icons/app.png"))
            .expect("embedded application artwork must decode")
            .into_rgba8(),
    )
});
thread_local! {
    static IMAGES: RefCell<BTreeMap<u32, Arc<gpui::RenderImage>>> = const { RefCell::new(BTreeMap::new()) };
}

pub(super) fn for_scale(scale: f32) -> Arc<gpui::RenderImage> {
    let pixels = pixel_size(scale);
    IMAGES.with_borrow_mut(|images| {
        images
            .entry(pixels)
            .or_insert_with(|| Arc::new(render_image(resample(&SOURCE, pixels))))
            .clone()
    })
}

fn render_image(mut rgba: RgbaImage) -> gpui::RenderImage {
    // GPUI's sprite atlas takes BGRA bytes with straight alpha.
    for pixel in rgba.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    gpui::RenderImage::new(vec![image::Frame::new(rgba)])
}

fn pixel_size(scale: f32) -> u32 {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    (POINTS * scale)
        .round()
        .clamp(1.0, SOURCE.width().min(SOURCE.height()) as f32) as u32
}

fn linear_premultiplied(source: &RgbaImage) -> Rgba32FImage {
    ImageBuffer::from_fn(source.width(), source.height(), |x, y| {
        let pixel = source.get_pixel(x, y);
        let alpha = f32::from(pixel[3]) / 255.0;
        Rgba([
            linear(pixel[0]) * alpha,
            linear(pixel[1]) * alpha,
            linear(pixel[2]) * alpha,
            alpha,
        ])
    })
}

fn linear(value: u8) -> f32 {
    let value = f32::from(value) / 255.0;
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn srgb(value: f32) -> u8 {
    let encoded = if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round().clamp(0.0, 255.0) as u8
}

fn resample(source: &Rgba32FImage, pixels: u32) -> RgbaImage {
    let filtered = image::imageops::resize(source, pixels, pixels, FilterType::Lanczos3);
    ImageBuffer::from_fn(pixels, pixels, |x, y| {
        let sample = filtered.get_pixel(x, y);
        // Lanczos can overshoot at an edge. Keep valid premultiplied bounds
        // before unpremultiplying, including fully transparent corners.
        let alpha = sample[3].clamp(0.0, 1.0);
        let alpha_byte = (alpha * 255.0).round() as u8;
        if alpha_byte == 0 {
            return Rgba([0, 0, 0, 0]);
        }
        Rgba([
            srgb(sample[0].clamp(0.0, alpha) / alpha),
            srgb(sample[1].clamp(0.0, alpha) / alpha),
            srgb(sample[2].clamp(0.0, alpha) / alpha),
            alpha_byte,
        ])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn high_frequency_details_are_averaged_in_linear_light() {
        let source = ImageBuffer::from_fn(64, 64, |x, y| {
            let value = if (x + y) % 2 == 0 { 255 } else { 0 };
            Rgba([value, value, value, 255])
        });
        let result = resample(&linear_premultiplied(&source), 8);
        // Equal black and white energy is about 188 in sRGB, rather than 128.
        for y in 1..7 {
            for x in 1..7 {
                let pixel = result.get_pixel(x, y);
                assert!((186..=190).contains(&pixel[0]), "{pixel:?}");
                assert_eq!(pixel[0], pixel[1]);
                assert_eq!(pixel[1], pixel[2]);
                assert_eq!(pixel[3], 255);
            }
        }
    }

    #[test]
    fn transparent_pixels_do_not_bleed_their_hidden_color() {
        let source = ImageBuffer::from_fn(64, 64, |x, _| {
            if x < 32 {
                Rgba([255, 0, 0, 255])
            } else {
                Rgba([0, 0, 255, 0])
            }
        });
        let result = resample(&linear_premultiplied(&source), 9);
        assert!(result.pixels().any(|p| p[3] > 0 && p[3] < 255));
        for pixel in result.pixels().filter(|p| p[3] > 0) {
            assert_eq!(pixel[0], 255);
            assert_eq!(pixel[1], 0);
            assert_eq!(pixel[2], 0);
        }
    }

    #[test]
    fn uniform_translucent_color_is_preserved() {
        let expected = Rgba([30, 90, 180, 77]);
        let source = ImageBuffer::from_pixel(64, 64, expected);
        let result = resample(&linear_premultiplied(&source), 11);
        for pixel in result.pixels() {
            for index in 0..4 {
                assert!(pixel[index].abs_diff(expected[index]) <= 1);
            }
        }
    }

    #[test]
    fn gpu_buffer_uses_bgra_without_premultiplying_again() {
        let source = ImageBuffer::from_pixel(1, 1, Rgba([10, 20, 230, 100]));
        let icon = render_image(source);
        assert_eq!(icon.as_bytes(0), Some([230, 20, 10, 100].as_slice()));
    }

    #[test]
    fn thumbnail_resolution_tracks_display_scale_and_reuses_the_cache() {
        for (scale, expected) in [(1.0, 44), (1.25, 55), (1.5, 66), (2.0, 88), (3.0, 132)] {
            let icon = for_scale(scale);
            assert!(Arc::ptr_eq(&icon, &for_scale(scale)));
            let size = icon.size(0);
            assert_eq!((size.width.0, size.height.0), (expected, expected));
        }
        assert_eq!(pixel_size(f32::NAN), 44);
        assert_eq!(pixel_size(f32::INFINITY), 44);
        assert_eq!(pixel_size(0.0), 44);
        assert_eq!(pixel_size(100.0), 512);
    }
}
