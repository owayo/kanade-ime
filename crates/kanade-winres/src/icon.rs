//! Icon images in the layouts Windows reads from resources and `.ico` files.
use crate::Error;

/// A square image in straight (not premultiplied) RGBA, rows from top to bottom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    size: u32,
    rgba: Vec<u8>,
}

impl Image {
    /// `rgba` holds `size * size` pixels. Icon directories store a size in one
    /// byte, which limits it to 255.
    pub fn new(size: u32, rgba: Vec<u8>) -> Result<Self, Error> {
        if !(1..=255).contains(&size) {
            return Err(Error::Icon("size must be between 1 and 255 pixels"));
        }
        if rgba.len() != (size * size * 4) as usize {
            return Err(Error::Icon("does not hold four bytes for every pixel"));
        }
        Ok(Self { size, rgba })
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// The bitmap of an `RT_ICON` resource and of an `.ico` entry: a header,
    /// the 32-bit colour rows and a 1-bit transparency mask, both bottom-up.
    pub fn bitmap(&self) -> Vec<u8> {
        let size = self.size as usize;
        // Mask rows are padded to 32 bits.
        let mask_row = size.div_ceil(32) * 4;
        let pixels = size * size * 4 + size * mask_row;
        let mut out = Vec::with_capacity(40 + pixels);
        // BITMAPINFOHEADER. Its height counts the colour rows and the mask rows.
        out.extend(40u32.to_le_bytes());
        out.extend(self.size.to_le_bytes());
        out.extend((self.size * 2).to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(32u16.to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend((pixels as u32).to_le_bytes());
        out.extend([0; 16]);
        for row in self.rgba.chunks_exact(size * 4).rev() {
            for &[red, green, blue, alpha] in row.as_chunks::<4>().0 {
                out.extend([blue, green, red, alpha]);
            }
        }
        // Windows draws from the alpha channel. The mask serves code that only
        // asks where the icon is: a set bit means nothing is drawn there.
        for row in self.rgba.chunks_exact(size * 4).rev() {
            let mut bits = vec![0u8; mask_row];
            for (x, pixel) in row.as_chunks::<4>().0.iter().enumerate() {
                if pixel[3] == 0 {
                    bits[x / 8] |= 0x80 >> (x % 8);
                }
            }
            out.extend(bits);
        }
        out
    }
}

/// The 12 bytes that open an entry of both directory layouts.
fn entry(out: &mut Vec<u8>, image: &Image, bitmap: usize) {
    out.extend([image.size as u8, image.size as u8, 0, 0]);
    out.extend(1u16.to_le_bytes());
    out.extend(32u16.to_le_bytes());
    out.extend((bitmap as u32).to_le_bytes());
}

fn directory(images: &[Image]) -> Result<Vec<u8>, Error> {
    let count = u16::try_from(images.len()).map_err(|_| Error::Icon("has too many images"))?;
    if count == 0 {
        return Err(Error::Icon("needs at least one image"));
    }
    let mut out = Vec::new();
    out.extend(0u16.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(count.to_le_bytes());
    Ok(out)
}

/// The data of an `RT_GROUP_ICON` resource. It names the `RT_ICON` resource
/// `first_id + n` as the bitmap of image `n`.
pub fn group(images: &[Image], first_id: u16) -> Result<Vec<u8>, Error> {
    let mut out = directory(images)?;
    for (position, image) in images.iter().enumerate() {
        let id = u16::try_from(position)
            .ok()
            .and_then(|position| first_id.checked_add(position))
            .ok_or(Error::Icon("image identifier does not fit in 16 bits"))?;
        entry(&mut out, image, image.bitmap().len());
        out.extend(id.to_le_bytes());
    }
    Ok(out)
}

/// A standalone `.ico` file, for looking at the images outside a binary.
pub fn file(images: &[Image]) -> Result<Vec<u8>, Error> {
    let mut out = directory(images)?;
    let bitmaps: Vec<_> = images.iter().map(Image::bitmap).collect();
    let mut offset = out.len() + 16 * images.len();
    for (image, bitmap) in images.iter().zip(&bitmaps) {
        entry(&mut out, image, bitmap.len());
        out.extend((offset as u32).to_le_bytes());
        offset += bitmap.len();
    }
    for bitmap in bitmaps {
        out.extend(bitmap);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(bytes: &[u8], at: usize) -> u32 {
        u32::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]))
    }

    fn long(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }

    /// 2x2: red, clear / half-clear green, blue.
    fn sample() -> Image {
        let rgba = [
            [255, 0, 0, 255],
            [9, 9, 9, 0],
            [0, 255, 0, 128],
            [0, 0, 255, 255],
        ];
        Image::new(2, rgba.concat()).unwrap()
    }

    #[test]
    fn bitmap_is_bottom_up_bgra_followed_by_the_mask() {
        let bitmap = sample().bitmap();
        assert_eq!(long(&bitmap, 0), 40);
        assert_eq!(long(&bitmap, 4), 2);
        assert_eq!(long(&bitmap, 8), 4, "height covers colour and mask");
        assert_eq!(word(&bitmap, 12), 1);
        assert_eq!(word(&bitmap, 14), 32);
        assert_eq!(long(&bitmap, 16), 0, "uncompressed");
        assert_eq!(long(&bitmap, 20), 16 + 8);
        assert_eq!(bitmap.len(), 40 + 16 + 8);
        assert_eq!(
            bitmap[40..56],
            [0, 255, 0, 128, 255, 0, 0, 255, 0, 0, 255, 255, 9, 9, 9, 0]
        );
        // Only the fully clear pixel is masked: the second pixel of the top row.
        assert_eq!(bitmap[56..64], [0, 0, 0, 0, 0x40, 0, 0, 0]);
    }

    #[test]
    fn mask_rows_are_padded_to_32_bits() {
        for (size, row) in [(16, 4), (32, 4), (33, 8), (48, 8)] {
            let image = Image::new(size, vec![0; (size * size * 4) as usize]).unwrap();
            let bitmap = image.bitmap();
            let size = size as usize;
            assert_eq!(bitmap.len(), 40 + size * size * 4 + size * row);
            assert!(bitmap[40 + size * size * 4..].chunks(row).all(|bits| {
                bits.iter().map(|byte| byte.count_ones()).sum::<u32>() == size as u32
            }));
        }
    }

    #[test]
    fn group_names_consecutive_bitmap_resources() {
        let images = [sample(), Image::new(3, vec![7; 36]).unwrap()];
        let group = group(&images, 5).unwrap();
        assert_eq!(group.len(), 6 + 2 * 14);
        assert_eq!(
            [word(&group, 0), word(&group, 2), word(&group, 4)],
            [0, 1, 2]
        );
        for (position, image) in images.iter().enumerate() {
            let at = 6 + position * 14;
            assert_eq!(
                group[at..at + 4],
                [image.size() as u8, image.size() as u8, 0, 0]
            );
            assert_eq!([word(&group, at + 4), word(&group, at + 6)], [1, 32]);
            assert_eq!(long(&group, at + 8) as usize, image.bitmap().len());
            assert_eq!(word(&group, at + 12) as usize, 5 + position);
        }
    }

    #[test]
    fn file_locates_every_bitmap() {
        let images = [sample(), Image::new(3, vec![7; 36]).unwrap()];
        let file = file(&images).unwrap();
        assert_eq!([word(&file, 0), word(&file, 2), word(&file, 4)], [0, 1, 2]);
        let mut end = 6 + 2 * 16;
        for (position, image) in images.iter().enumerate() {
            let at = 6 + position * 16;
            let bitmap = image.bitmap();
            let offset = long(&file, at + 12) as usize;
            assert_eq!(offset, end);
            assert_eq!(file[offset..offset + bitmap.len()], bitmap);
            end += bitmap.len();
        }
        assert_eq!(file.len(), end);
    }

    #[test]
    fn rejects_images_the_formats_cannot_describe() {
        assert!(Image::new(0, Vec::new()).is_err());
        assert!(Image::new(256, vec![0; 256 * 256 * 4]).is_err());
        assert!(Image::new(2, vec![0; 15]).is_err());
        assert!(group(&[], 1).is_err());
        assert!(file(&[]).is_err());
        assert!(group(&[sample(), sample()], u16::MAX).is_err());
    }
}
