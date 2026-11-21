//! Kanade's brand icon for the Windows input switcher and the taskbar's input
//! indicator.
//!
//! Microsoft's requirements for input methods ask for a white box holding a
//! black glyph, an outer stroke of one pixel in black at half opacity, and six
//! sizes with an alpha channel. The template is the mark the macOS input
//! source uses: an opaque silhouette with the glyph cut out of it. The
//! silhouette becomes the white box, the cutout becomes the black glyph, and
//! the stroke is added around the silhouette.
use crate::{Error, icon::Image};

/// The pixel sizes Windows asks an input method to provide.
pub const SIZES: [u32; 6] = [16, 20, 24, 32, 40, 48];

/// Samples along each axis of an output pixel.
const SAMPLES: usize = 16;

/// The generated template carries faint noise where it is clear and stops just
/// short of full opacity where it is solid. Alpha is stretched between these
/// two values so that clear and solid areas are exact.
const CLEAR: u8 = 16;
const SOLID: u8 = 240;

/// Renders the icon in every size of [`SIZES`] from the template PNG.
pub fn render(template_png: &[u8]) -> Result<Vec<Image>, Error> {
    let template = Template::decode(template_png)?;
    SIZES.iter().map(|&size| template.render(size)).collect()
}

struct Template {
    /// Width of the decoded picture.
    width: usize,
    /// How much of each pixel the mark covers, `0..=255`.
    body: Vec<u8>,
    /// Pixels of the silhouette: the mark and everything it encloses.
    inside: Vec<bool>,
    /// The silhouette's box: left, top, width, height.
    bounds: [usize; 4],
}

impl Template {
    fn decode(png: &[u8]) -> Result<Self, Error> {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info()?;
        let size = reader
            .output_buffer_size()
            .ok_or(Error::Template("is too large"))?;
        let mut pixels = vec![0; size];
        let frame = reader.next_frame(&mut pixels)?;
        let channels = match frame.color_type {
            png::ColorType::Rgba => 4,
            png::ColorType::GrayscaleAlpha => 2,
            _ => return Err(Error::Template("has no alpha channel")),
        };
        let (width, height) = (frame.width as usize, frame.height as usize);
        let body: Vec<u8> = pixels
            .chunks_exact(channels)
            .take(width * height)
            .map(|pixel| coverage(pixel[channels - 1]))
            .collect();
        if body.len() != width * height {
            return Err(Error::Template("is truncated"));
        }
        let inside = enclosed(&body, width, height);
        let bounds = bounds(&inside, width, height).ok_or(Error::Template("is empty"))?;
        let [left, top, wide, high] = bounds;
        let (mut silhouette, mut boxed, mut glyph) = (0u64, 0u64, 0u64);
        for (pixel, _) in inside.iter().enumerate().filter(|(_, inside)| **inside) {
            let (x, y) = (pixel % width, pixel / width);
            silhouette += 1;
            if (left..left + wide).contains(&x) && (top..top + high).contains(&y) {
                boxed += 1;
                glyph += 255 - u64::from(body[pixel]);
            }
        }
        // The stroke follows the silhouette, but the edges of the icon are
        // straight: a round or slanted silhouette would be cut off by them.
        if boxed * 100 < silhouette * 99 || boxed * 10 < (wide * high) as u64 * 9 {
            return Err(Error::Template("silhouette is not a box"));
        }
        // Each axis is scaled separately so that the box's edges land on pixel
        // boundaries. That is invisible only while the box is close to square.
        if wide.abs_diff(high) * 50 > wide.max(high) {
            return Err(Error::Template("silhouette is not square"));
        }
        // A gap in the mark joins the cutout to the surroundings, which leaves
        // no glyph. A cutout that swallows the box leaves no box.
        if glyph * 100 < boxed * 255 || glyph * 5 > boxed * 255 * 4 {
            return Err(Error::Template("has no glyph enclosed by its silhouette"));
        }
        Ok(Self {
            width,
            body,
            inside,
            bounds,
        })
    }

    /// Resamples the silhouette's box to `side * side` cells by area.
    /// Returns, per cell, the weight inside the silhouette and that weight
    /// times the body coverage, followed by the weight of a whole cell.
    fn resample(&self, side: usize) -> (Vec<u64>, Vec<u64>, u64) {
        let [left, top, wide, high] = self.bounds;
        let columns = overlaps(wide, side);
        let rows = overlaps(high, side);
        let mut inside = vec![0u64; side * side];
        let mut white = vec![0u64; side * side];
        for &(source_y, y, tall) in &rows {
            let source_row = (top + source_y) * self.width + left;
            for &(source_x, x, broad) in &columns {
                let pixel = source_row + source_x;
                if self.inside[pixel] {
                    let weight = tall * broad;
                    inside[y * side + x] += weight;
                    white[y * side + x] += weight * u64::from(self.body[pixel]);
                }
            }
        }
        (inside, white, (wide * high) as u64)
    }

    fn render(&self, size: u32) -> Result<Image, Error> {
        let pixels = size as usize;
        if pixels < 3 {
            return Err(Error::Icon("is too small to hold a stroke and a box"));
        }
        // The box fills the icon except for the stroke around it.
        let art = (pixels - 2) * SAMPLES;
        let side = pixels * SAMPLES;
        let (inside, white, whole) = self.resample(art);
        let cell = |x: usize, y: usize| {
            let range = SAMPLES..SAMPLES + art;
            (range.contains(&x) && range.contains(&y)).then(|| (y - SAMPLES) * art + (x - SAMPLES))
        };
        let mut marked = vec![false; side * side];
        for y in 0..side {
            for x in 0..side {
                marked[y * side + x] = cell(x, y).is_some_and(|cell| inside[cell] * 2 >= whole);
            }
        }
        let distance = distance_to(&marked, side);
        // Per output pixel: the weight of the box, the weight of its white
        // part, and the opacity the stroke adds outside the box. The first two
        // stay whole numbers so that a plain box and a plain stroke are exact.
        let mut boxed = vec![0u64; pixels * pixels];
        let mut lit = vec![0u64; pixels * pixels];
        let mut stroked = vec![0.0f64; pixels * pixels];
        for y in 0..side {
            for x in 0..side {
                let pixel = (y / SAMPLES) * pixels + x / SAMPLES;
                let covered = cell(x, y).map_or(0, |cell| {
                    boxed[pixel] += inside[cell];
                    lit[pixel] += white[cell];
                    inside[cell]
                });
                // The stroke is everything within one output pixel of the box.
                let stroke = (SAMPLES as f64 + 1.0 - distance[y * side + x]).clamp(0.0, 1.0);
                stroked[pixel] += (1.0 - covered as f64 / whole as f64) * 0.5 * stroke;
            }
        }
        let samples = (SAMPLES * SAMPLES) as f64;
        let mut rgba = Vec::with_capacity(pixels * pixels * 4);
        for ((boxed, lit), stroked) in boxed.into_iter().zip(lit).zip(stroked) {
            let alpha = boxed as f64 / whole as f64 + stroked;
            // White is the only colour that contributes light: the glyph and
            // the stroke are black.
            let level = if alpha > 0.0 {
                (lit as f64 / whole as f64 / alpha).round().min(255.0) as u8
            } else {
                0
            };
            rgba.extend([level, level, level, (255.0 * alpha / samples).round() as u8]);
        }
        Image::new(size, rgba)
    }
}

fn coverage(alpha: u8) -> u8 {
    let span = u32::from(SOLID - CLEAR);
    let above = u32::from(alpha.clamp(CLEAR, SOLID) - CLEAR);
    ((above * 255 + span / 2) / span) as u8
}

/// Marks the silhouette: every pixel that cannot be reached from the picture's
/// edge without crossing solid pixels. The cutout is enclosed by the mark, so
/// it belongs to the silhouette; the antialiased fringe around the mark does not.
fn enclosed(body: &[u8], width: usize, height: usize) -> Vec<bool> {
    let mut outside = vec![false; body.len()];
    let mut pending = Vec::new();
    let visit = |pixel: usize, outside: &mut Vec<bool>, pending: &mut Vec<usize>| {
        if body[pixel] != 255 && !outside[pixel] {
            outside[pixel] = true;
            pending.push(pixel);
        }
    };
    for x in 0..width {
        visit(x, &mut outside, &mut pending);
        visit((height - 1) * width + x, &mut outside, &mut pending);
    }
    for y in 0..height {
        visit(y * width, &mut outside, &mut pending);
        visit(y * width + width - 1, &mut outside, &mut pending);
    }
    while let Some(pixel) = pending.pop() {
        let (x, y) = (pixel % width, pixel / width);
        if x > 0 {
            visit(pixel - 1, &mut outside, &mut pending);
        }
        if x + 1 < width {
            visit(pixel + 1, &mut outside, &mut pending);
        }
        if y > 0 {
            visit(pixel - width, &mut outside, &mut pending);
        }
        if y + 1 < height {
            visit(pixel + width, &mut outside, &mut pending);
        }
    }
    outside.into_iter().map(|outside| !outside).collect()
}

/// The box of the silhouette: left, top, width, height. Its edges are the
/// outermost rows and columns that the silhouette fills at least half as far
/// as its fullest one, so a stray pixel beyond a straight edge does not move it.
fn bounds(inside: &[bool], width: usize, height: usize) -> Option<[usize; 4]> {
    let mut rows = vec![0usize; height];
    let mut columns = vec![0usize; width];
    for (pixel, _) in inside.iter().enumerate().filter(|(_, inside)| **inside) {
        rows[pixel / width] += 1;
        columns[pixel % width] += 1;
    }
    let span = |filled: &[usize]| {
        let fullest = *filled.iter().max()?;
        let edge = |&count: &usize| fullest > 0 && count * 2 >= fullest;
        let first = filled.iter().position(edge)?;
        let last = filled.iter().rposition(edge)?;
        Some((first, last - first + 1))
    };
    let (top, high) = span(&rows)?;
    let (left, wide) = span(&columns)?;
    Some([left, top, wide, high])
}

/// Where `from` equal source cells and `to` equal target cells share length
/// when both rows span the same extent: `(source, target, length)`. Lengths
/// are whole numbers in a unit that makes a target cell `from` long.
fn overlaps(from: usize, to: usize) -> Vec<(usize, usize, u64)> {
    let mut shared = Vec::with_capacity(from + to);
    let (mut source, mut target, mut position) = (0, 0, 0);
    while source < from && target < to {
        let source_end = ((source + 1) * to) as u64;
        let target_end = ((target + 1) * from) as u64;
        let end = source_end.min(target_end);
        shared.push((source, target, end - position));
        position = end;
        source += usize::from(end == source_end);
        target += usize::from(end == target_end);
    }
    shared
}

/// The distance from every cell of a `side * side` grid to the nearest marked
/// cell, in cells and capped at `side`. A marked cell is at distance zero.
///
/// Felzenszwalb and Huttenlocher's exact transform: distances along columns,
/// then the lower envelope of the parabolas they define along each row.
fn distance_to(marked: &[bool], side: usize) -> Vec<f64> {
    let far = side as f64;
    let mut squared = vec![0.0f64; side * side];
    for x in 0..side {
        let mut distance = far;
        for y in 0..side {
            distance = if marked[y * side + x] {
                0.0
            } else {
                (distance + 1.0).min(far)
            };
            squared[y * side + x] = distance;
        }
        distance = far;
        for y in (0..side).rev() {
            distance = if marked[y * side + x] {
                0.0
            } else {
                (distance + 1.0).min(far)
            };
            let nearest = squared[y * side + x].min(distance);
            squared[y * side + x] = nearest * nearest;
        }
    }
    let mut result = vec![0.0f64; side * side];
    // `vertex[k]` is the column whose parabola is lowest from `start[k]` on.
    let mut vertex = vec![0usize; side];
    let mut start = vec![0.0f64; side + 1];
    for (row, result) in squared
        .chunks_exact(side)
        .zip(result.chunks_exact_mut(side))
    {
        let mut last = 0;
        vertex[0] = 0;
        start[0] = f64::NEG_INFINITY;
        start[1] = f64::INFINITY;
        for column in 1..side {
            loop {
                let other = vertex[last];
                let crossing = ((row[column] + (column * column) as f64)
                    - (row[other] + (other * other) as f64))
                    / (2 * (column - other)) as f64;
                // The first parabola starts at negative infinity, so the
                // search never empties the envelope.
                if crossing <= start[last] {
                    last -= 1;
                } else {
                    last += 1;
                    vertex[last] = column;
                    start[last] = crossing;
                    start[last + 1] = f64::INFINITY;
                    break;
                }
            }
        }
        let mut current = 0;
        for (column, result) in result.iter_mut().enumerate() {
            while start[current + 1] < column as f64 {
                current += 1;
            }
            let across = column as f64 - vertex[current] as f64;
            *result = (across * across + row[vertex[current]]).sqrt();
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../assets/icons/input-template.png"
    );

    fn png(width: u32, height: u32, alpha: impl Fn(u32, u32) -> u8) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                pixels.extend([0, 0, 0, alpha(x, y)]);
            }
        }
        writer.write_image_data(&pixels).unwrap();
        writer.finish().unwrap();
        bytes
    }

    /// A 140-pixel square mark inside a margin, with a bar cut out of it.
    fn boxed_bar(x: u32, y: u32) -> u8 {
        let mark = (20..160).contains(&x) && (30..170).contains(&y);
        let bar = (60..120).contains(&x) && (80..100).contains(&y);
        if mark && !bar { 255 } else { 0 }
    }

    fn pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * image.size() + x) * 4) as usize;
        image.rgba()[at..at + 4].try_into().unwrap()
    }

    #[test]
    fn a_square_template_becomes_a_stroked_white_box_with_a_black_glyph() {
        let images = render(&png(180, 200, boxed_bar)).unwrap();
        assert_eq!(images.iter().map(Image::size).collect::<Vec<_>>(), SIZES);
        // 16 pixels: the 140-pixel mark fills the inner 14, ten source pixels each.
        let image = &images[0];
        for at in 1..15 {
            for (x, y) in [(at, 0), (at, 15), (0, at), (15, at)] {
                assert_eq!(pixel(image, x, y), [0, 0, 0, 128], "stroke at {x},{y}");
            }
        }
        // The stroke rounds a square corner: a quarter disc of one pixel.
        for (x, y) in [(0, 0), (15, 0), (0, 15), (15, 15)] {
            let [red, green, blue, alpha] = pixel(image, x, y);
            assert_eq!([red, green, blue], [0, 0, 0]);
            assert!((95..=105).contains(&alpha), "corner alpha {alpha}");
        }
        // The bar covers source 60..120 by 80..100: output 5..11 by 6..8.
        for y in 1..15 {
            for x in 1..15 {
                let glyph = (5..11).contains(&x) && (6..8).contains(&y);
                let expected = if glyph { [0, 0, 0, 255] } else { [255; 4] };
                assert_eq!(pixel(image, x, y), expected, "box at {x},{y}");
            }
        }
    }

    #[test]
    fn partly_covered_pixels_blend_the_glyph_into_the_box() {
        // A bar from 85 to 100 covers half of the output row that starts at 80.
        let half = |x, y| {
            let mark = (20..160).contains(&x) && (30..170).contains(&y);
            let bar = (60..120).contains(&x) && (85..100).contains(&y);
            if mark && !bar { 255 } else { 0 }
        };
        let images = render(&png(180, 200, half)).unwrap();
        assert_eq!(pixel(&images[0], 7, 6), [128, 128, 128, 255]);
        assert_eq!(pixel(&images[0], 7, 7), [0, 0, 0, 255]);
        assert_eq!(pixel(&images[0], 7, 5), [255; 4]);
    }

    #[test]
    fn template_noise_and_near_opacity_do_not_tint_the_icon() {
        // Like the real template: solid areas at 253, clear areas up to 12.
        let noisy = |x, y| {
            if boxed_bar(x, y) == 255 {
                253
            } else {
                (x * 7 + y) as u8 % 13
            }
        };
        assert_eq!(
            render(&png(180, 200, noisy)).unwrap(),
            render(&png(180, 200, boxed_bar)).unwrap()
        );
    }

    #[test]
    fn unusable_templates_are_refused() {
        let message =
            |alpha: &dyn Fn(u32, u32) -> u8| render(&png(180, 200, alpha)).unwrap_err().to_string();
        assert!(message(&|_, _| 0).contains("is empty"));
        assert!(
            message(
                &|x, y| if (20..160).contains(&x) && (30..100).contains(&y) {
                    255
                } else {
                    0
                }
            )
            .contains("not square")
        );
        // A disc: the icon's straight edges would cut it off.
        assert!(
            message(&|x, y| {
                let (across, down) = (x.abs_diff(90), y.abs_diff(100));
                let ring = (400..4900).contains(&(across * across + down * down));
                if ring { 255 } else { 0 }
            })
            .contains("not a box")
        );
        // No cutout.
        assert!(
            message(
                &|x, y| if (20..160).contains(&x) && (30..170).contains(&y) {
                    255
                } else {
                    0
                }
            )
            .contains("no glyph")
        );
        // A slit joins the cutout to the surroundings.
        assert!(
            message(&|x, y| {
                let slit = (88..92).contains(&x) && y < 90;
                if boxed_bar(x, y) == 255 && !slit {
                    255
                } else {
                    0
                }
            })
            .contains("no glyph")
        );
        assert!(matches!(render(b"not a png"), Err(Error::Decode(_))));
    }

    #[test]
    fn distances_are_exact() {
        let side = 9;
        let mut marked = vec![false; side * side];
        marked[4 * side + 4] = true;
        marked[side - 1] = true;
        let distance = distance_to(&marked, side);
        for y in 0..side {
            for x in 0..side {
                let to = |mx: usize, my: usize| {
                    ((x.abs_diff(mx).pow(2) + y.abs_diff(my).pow(2)) as f64).sqrt()
                };
                assert_eq!(distance[y * side + x], to(4, 4).min(to(side - 1, 0)));
            }
        }
        assert!(distance_to(&[false; 16], 4).iter().all(|&far| far == 4.0));
    }

    #[test]
    fn overlaps_partition_both_rows() {
        for (from, to) in [(1045, 224), (10, 4), (3, 7), (5, 5)] {
            let shared = overlaps(from, to);
            for target in 0..to {
                let length: u64 = shared.iter().filter(|s| s.1 == target).map(|s| s.2).sum();
                assert_eq!(length, from as u64);
            }
            for source in 0..from {
                let length: u64 = shared.iter().filter(|s| s.0 == source).map(|s| s.2).sum();
                assert_eq!(length, to as u64);
            }
        }
    }

    #[test]
    fn the_shipped_template_meets_the_windows_requirements() {
        let images = render(&std::fs::read(TEMPLATE).unwrap()).unwrap();
        assert_eq!(images.iter().map(Image::size).collect::<Vec<_>>(), SIZES);
        for image in &images {
            let size = image.size();
            let pixels = image.rgba().as_chunks::<4>().0;
            // Black and white only.
            assert!(
                pixels
                    .iter()
                    .all(|[red, green, blue, _]| red == green && green == blue)
            );
            // One pixel of half-opaque black along every straight edge. The
            // template's bottom edge is one source pixel short in places.
            for at in size / 4..size * 3 / 4 {
                for (x, y) in [(at, 0), (at, size - 1), (0, at), (size - 1, at)] {
                    let [level, _, _, alpha] = pixel(image, x, y);
                    assert_eq!(level, 0, "{size}: stroke colour at {x},{y}");
                    assert!(
                        (126..=128).contains(&alpha),
                        "{size}: stroke alpha {alpha} at {x},{y}"
                    );
                }
                // Directly inside it, an opaque box.
                for (x, y) in [(at, 1), (at, size - 2), (1, at), (size - 2, at)] {
                    let alpha = pixel(image, x, y)[3];
                    assert!(alpha >= 253, "{size}: box alpha {alpha} at {x},{y}");
                }
            }
            // The rounded corners stay clear, the box is white and holds a glyph.
            assert!(pixel(image, 0, 0)[3] < 64, "{size}: corner");
            let opaque: Vec<u8> = pixels
                .iter()
                .filter(|p| p[3] == 255)
                .map(|p| p[0])
                .collect();
            let share = |matches: &dyn Fn(u8) -> bool| {
                opaque.iter().filter(|&&level| matches(level)).count() * 100 / opaque.len()
            };
            assert!(share(&|level| level >= 224) >= 30, "{size}: white share");
            assert!(share(&|level| level < 128) >= 10, "{size}: glyph share");
        }
    }
}
