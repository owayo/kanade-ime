//! Writes the Windows brand icon as files for a visual check:
//!
//! ```text
//! cargo run -p kanade-winres --example brand-icon-preview -- <output directory>
//! ```
//!
//! `kanade-brand.ico` holds the images that are linked into the input DLL.
//! `kanade-brand-preview.png` shows every size on a light and a dark taskbar
//! colour, at its own size and enlarged.
use kanade_winres::{brand_icon, icon};
use std::{error::Error, fs, io::BufWriter, path::PathBuf};

const TEMPLATE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/icons/input-template.png"
);
/// Windows 11 taskbar colours in the light and the dark theme.
const BACKGROUNDS: [[u8; 3]; 2] = [[243, 243, 243], [32, 32, 32]];
const ENLARGED: u32 = 8;
const GAP: u32 = 16;

struct Canvas {
    width: u32,
    height: u32,
    rgb: Vec<u8>,
}

impl Canvas {
    fn fill(&mut self, x: u32, y: u32, width: u32, height: u32, colour: [u8; 3]) {
        for row in y..(y + height).min(self.height) {
            for column in x..(x + width).min(self.width) {
                let at = ((row * self.width + column) * 3) as usize;
                self.rgb[at..at + 3].copy_from_slice(&colour);
            }
        }
    }

    /// Composites `image` at `scale` over whatever is already there.
    fn draw(&mut self, image: &icon::Image, x: u32, y: u32, scale: u32) {
        for (index, pixel) in image.rgba().as_chunks::<4>().0.iter().enumerate() {
            let (column, row) = (index as u32 % image.size(), index as u32 / image.size());
            let at = (((y + row * scale) * self.width + x + column * scale) * 3) as usize;
            let alpha = u32::from(pixel[3]);
            let mut colour = [0; 3];
            for channel in 0..3 {
                let under = u32::from(self.rgb[at + channel]);
                let over = u32::from(pixel[channel]);
                colour[channel] = ((over * alpha + under * (255 - alpha) + 127) / 255) as u8;
            }
            self.fill(x + column * scale, y + row * scale, scale, scale, colour);
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let directory = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("usage: brand-icon-preview <output directory>")?,
    );
    fs::create_dir_all(&directory)?;
    let images = brand_icon::render(&fs::read(TEMPLATE)?)?;
    fs::write(directory.join("kanade-brand.ico"), icon::file(&images)?)?;

    let column = |size: u32| size + GAP + size * ENLARGED + 2 * GAP;
    let band: u32 = images.iter().map(|image| column(image.size())).sum::<u32>() + GAP;
    let tallest = images.iter().map(icon::Image::size).max().unwrap_or(0) * ENLARGED + 2 * GAP;
    let mut canvas = Canvas {
        width: band,
        height: tallest * BACKGROUNDS.len() as u32,
        rgb: vec![0; (band * tallest * BACKGROUNDS.len() as u32 * 3) as usize],
    };
    for (row, background) in BACKGROUNDS.into_iter().enumerate() {
        let top = row as u32 * tallest;
        canvas.fill(0, top, band, tallest, background);
        let mut left = GAP;
        for image in &images {
            canvas.draw(image, left, top + GAP, 1);
            canvas.draw(image, left + image.size() + GAP, top + GAP, ENLARGED);
            left += column(image.size());
        }
    }
    let file = BufWriter::new(fs::File::create(
        directory.join("kanade-brand-preview.png"),
    )?);
    let mut encoder = png::Encoder::new(file, canvas.width, canvas.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&canvas.rgb)?;
    writer.finish()?;
    println!("wrote {}", directory.display());
    Ok(())
}
