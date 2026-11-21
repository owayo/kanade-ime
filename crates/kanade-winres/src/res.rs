//! The binary resource file (`.res`) that resource compilers write. `link.exe`
//! and `lld-link` take it as an input file, so no resource compiler has to run.
use crate::{
    Error,
    icon::{self, Image},
};

pub const RT_ICON: u16 = 3;
pub const RT_GROUP_ICON: u16 = 14;

/// A resource named by numbers, in the neutral language.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resource {
    pub kind: u16,
    pub id: u16,
    pub data: Vec<u8>,
}

/// One icon: its directory as group `group_id` and its bitmaps as the
/// `RT_ICON` resources `first_bitmap_id..`.
///
/// Programs that address an icon by position count the groups of a file in
/// ascending identifier order, starting at zero.
pub fn icon_group(
    group_id: u16,
    first_bitmap_id: u16,
    images: &[Image],
) -> Result<Vec<Resource>, Error> {
    let directory = icon::group(images, first_bitmap_id)?;
    let mut resources: Vec<_> = (first_bitmap_id..)
        .zip(images)
        .map(|(id, image)| Resource {
            kind: RT_ICON,
            id,
            data: image.bitmap(),
        })
        .collect();
    resources.push(Resource {
        kind: RT_GROUP_ICON,
        id: group_id,
        data: directory,
    });
    Ok(resources)
}

/// Flags a resource compiler records for 16-bit loaders. Win32 ignores them.
fn memory_flags(kind: u16) -> u16 {
    const MOVEABLE: u16 = 0x0010;
    const PURE: u16 = 0x0020;
    const DISCARDABLE: u16 = 0x1000;
    match kind {
        RT_ICON => DISCARDABLE | MOVEABLE,
        RT_GROUP_ICON => DISCARDABLE | PURE | MOVEABLE,
        _ => PURE | MOVEABLE,
    }
}

fn header(out: &mut Vec<u8>, data: usize, kind: u16, id: u16, flags: u16) -> Result<(), Error> {
    let data = u32::try_from(data).map_err(|_| Error::Icon("resource is too large"))?;
    out.extend(data.to_le_bytes());
    // The header is 32 bytes when the type and the name are both numbers.
    out.extend(32u32.to_le_bytes());
    // 0xffff announces a number where a name could stand.
    for value in [0xffff, kind, 0xffff, id] {
        out.extend(value.to_le_bytes());
    }
    out.extend(0u32.to_le_bytes()); // data version
    out.extend(flags.to_le_bytes());
    out.extend(0u16.to_le_bytes()); // language: neutral
    out.extend(0u32.to_le_bytes()); // version
    out.extend(0u32.to_le_bytes()); // characteristics
    Ok(())
}

/// Serializes `resources` as a resource file.
pub fn file(resources: &[Resource]) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    // An empty entry opens every 32-bit resource file.
    header(&mut out, 0, 0, 0, 0)?;
    for resource in resources {
        header(
            &mut out,
            resource.data.len(),
            resource.kind,
            resource.id,
            memory_flags(resource.kind),
        )?;
        out.extend(&resource.data);
        // Every entry starts on a 32-bit boundary.
        out.resize(out.len().next_multiple_of(4), 0);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads a resource file back: `(type, name, language, data)` per entry.
    fn parse(file: &[u8]) -> Vec<(u16, u16, u16, Vec<u8>)> {
        let long = |at: usize| {
            u32::from_le_bytes([file[at], file[at + 1], file[at + 2], file[at + 3]]) as usize
        };
        let word = |at: usize| u16::from_le_bytes([file[at], file[at + 1]]);
        let mut entries = Vec::new();
        let mut at = 0;
        while at < file.len() {
            assert_eq!(at % 4, 0, "entries are aligned");
            let (data, header) = (long(at), long(at + 4));
            assert_eq!(header, 32);
            assert_eq!([word(at + 8), word(at + 12)], [0xffff, 0xffff]);
            entries.push((
                word(at + 10),
                word(at + 14),
                word(at + 22),
                file[at + header..at + header + data].to_vec(),
            ));
            at = (at + header + data).next_multiple_of(4);
        }
        assert_eq!(at, file.len());
        entries
    }

    #[test]
    fn file_starts_with_the_empty_marker_entry() {
        let file = file(&[]).unwrap();
        assert_eq!(
            file,
            [
                0, 0, 0, 0, 32, 0, 0, 0, 0xff, 0xff, 0, 0, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0
            ]
        );
    }

    #[test]
    fn entries_keep_their_data_and_are_padded() {
        let resources = [
            Resource {
                kind: RT_ICON,
                id: 7,
                data: vec![1, 2, 3, 4, 5],
            },
            Resource {
                kind: RT_GROUP_ICON,
                id: 9,
                data: vec![6; 8],
            },
        ];
        let file = file(&resources).unwrap();
        assert_eq!(file.len(), 32 + (32 + 8) + (32 + 8));
        // Memory flags sit after the data version.
        assert_eq!(file[32 + 20..32 + 22], 0x1010u16.to_le_bytes());
        assert_eq!(file[72 + 20..72 + 22], 0x1030u16.to_le_bytes());
        assert_eq!(
            parse(&file),
            [
                (0, 0, 0, Vec::new()),
                (RT_ICON, 7, 0, vec![1, 2, 3, 4, 5]),
                (RT_GROUP_ICON, 9, 0, vec![6; 8]),
            ]
        );
    }

    #[test]
    fn icon_group_pairs_the_directory_with_its_bitmaps() {
        let images = [
            Image::new(1, vec![1, 2, 3, 4]).unwrap(),
            Image::new(2, vec![5; 16]).unwrap(),
        ];
        let resources = icon_group(1, 10, &images).unwrap();
        assert_eq!(
            resources
                .iter()
                .map(|resource| (resource.kind, resource.id))
                .collect::<Vec<_>>(),
            [(RT_ICON, 10), (RT_ICON, 11), (RT_GROUP_ICON, 1)]
        );
        assert_eq!(resources[0].data, images[0].bitmap());
        assert_eq!(resources[1].data, images[1].bitmap());
        assert_eq!(resources[2].data, icon::group(&images, 10).unwrap());
        assert_eq!(parse(&file(&resources).unwrap()).len(), 4);
    }
}
