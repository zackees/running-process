//! Loaded-image inventory for the current Linux process (#974).
//!
//! Two loader views are joined here: `/proc/self/maps` says which files back
//! which address ranges, and `dl_iterate_phdr` says which loaded objects carry
//! which GNU build id. Reading the object files themselves stays with the
//! caller.

use std::collections::BTreeMap;
use std::ops::Range;

use crate::platform::process::{LoadedImage, LoadedImageBackingFile, LoadedImageFormat};

struct LoadedElfIdentity {
    load_bias: u64,
    mapped_ranges: Vec<Range<u64>>,
    build_id: Vec<u8>,
}

fn loaded_elf_identities() -> Vec<LoadedElfIdentity> {
    unsafe extern "C" fn visit(
        info: *mut libc::dl_phdr_info,
        _size: libc::size_t,
        data: *mut libc::c_void,
    ) -> libc::c_int {
        const MAX_NOTE_BYTES: usize = 1024 * 1024;
        let info = unsafe { &*info };
        let out = unsafe { &mut *data.cast::<Vec<LoadedElfIdentity>>() };
        if info.dlpi_phdr.is_null() || info.dlpi_phnum == 0 {
            return 0;
        }
        // libc exposes Elf_Addr as u64 on our 64-bit CI hosts and as a
        // narrower integer on 32-bit Linux; this widening keeps both valid.
        #[allow(clippy::unnecessary_cast)]
        let load_bias = info.dlpi_addr as u64;
        let headers =
            unsafe { std::slice::from_raw_parts(info.dlpi_phdr, usize::from(info.dlpi_phnum)) };
        let mapped_ranges = headers
            .iter()
            .filter(|header| header.p_type == libc::PT_LOAD && header.p_memsz > 0)
            .filter_map(|header| {
                let start = load_bias.checked_add(header.p_vaddr)?;
                let end = start.checked_add(header.p_memsz)?;
                Some(start..end)
            })
            .collect::<Vec<_>>();
        for header in headers {
            if header.p_type != libc::PT_NOTE {
                continue;
            }
            let Ok(length) = usize::try_from(header.p_memsz) else {
                continue;
            };
            if length == 0 || length > MAX_NOTE_BYTES {
                continue;
            }
            let Some(address) = load_bias.checked_add(header.p_vaddr) else {
                continue;
            };
            let Some(note_end) = address.checked_add(length as u64) else {
                continue;
            };
            let is_mapped = headers.iter().any(|load| {
                if load.p_type != libc::PT_LOAD || load.p_flags & libc::PF_R == 0 {
                    return false;
                }
                let Some(start) = load_bias.checked_add(load.p_vaddr) else {
                    return false;
                };
                let Some(end) = start.checked_add(load.p_memsz) else {
                    return false;
                };
                address >= start && note_end <= end
            });
            if address == 0 || !is_mapped {
                continue;
            }
            let notes = unsafe { std::slice::from_raw_parts(address as *const u8, length) };
            if let Some(build_id) = super::gnu_build_id_from_notes(notes) {
                out.push(LoadedElfIdentity {
                    load_bias,
                    mapped_ranges,
                    build_id: build_id.to_vec(),
                });
                break;
            }
        }
        0
    }

    let mut out = Vec::new();
    unsafe {
        libc::dl_iterate_phdr(
            Some(visit),
            (&mut out as *mut Vec<LoadedElfIdentity>).cast::<libc::c_void>(),
        );
    }
    out
}

fn next_maps_field(input: &str) -> Option<(&str, &str)> {
    let input = input.trim_start();
    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    (!input.is_empty()).then_some((&input[..end], &input[end..]))
}

struct LinuxImage {
    mapped_ranges: Vec<Range<u64>>,
    executable_ranges: Vec<Range<u64>>,
    path: String,
    device_major: u64,
    device_minor: u64,
    inode: String,
}

type LinuxImageKey = (String, String, String, u64);

struct LinuxMapping {
    range: Range<u64>,
    executable: bool,
}

fn linux_images(maps: &str) -> Vec<LinuxImage> {
    // (path, device, inode, load instance) -> individual mapped ranges.
    let mut images: BTreeMap<LinuxImageKey, Vec<LinuxMapping>> = BTreeMap::new();
    for line in maps.lines() {
        let Some((range, rest)) = next_maps_field(line) else {
            continue;
        };
        let Some((perms, rest)) = next_maps_field(rest) else {
            continue;
        };
        let Some((offset, rest)) = next_maps_field(rest) else {
            continue;
        };
        let Some((dev, rest)) = next_maps_field(rest) else {
            continue;
        };
        let Some((inode, rest)) = next_maps_field(rest) else {
            continue;
        };
        let path = rest.trim_start();
        if !path.starts_with('/') {
            continue;
        }
        if path.ends_with(" (deleted)") {
            // Reopening the same pathname could read a replacement build,
            // producing plausible but wrong unwind rules. A deleted mapping
            // is safer left raw.
            continue;
        }
        let path = path.to_owned();
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end), Ok(offset)) = (
            u64::from_str_radix(start, 16),
            u64::from_str_radix(end, 16),
            u64::from_str_radix(offset, 16),
        ) else {
            continue;
        };
        let candidate_base = start.saturating_sub(offset);
        images
            .entry((path, dev.to_owned(), inode.to_owned(), candidate_base))
            .or_default()
            .push(LinuxMapping {
                range: start..end,
                executable: perms.as_bytes().get(2) == Some(&b'x'),
            });
    }

    images
        .into_iter()
        .filter_map(|((path, device, inode, _load_bias), mut mappings)| {
            let (major, minor) = device.split_once(':')?;
            let device_major = u64::from_str_radix(major, 16).ok()?;
            let device_minor = u64::from_str_radix(minor, 16).ok()?;
            mappings.sort_by_key(|mapping| mapping.range.start);
            Some(LinuxImage {
                mapped_ranges: mappings
                    .iter()
                    .map(|mapping| mapping.range.clone())
                    .collect(),
                executable_ranges: mappings
                    .into_iter()
                    .filter_map(|mapping| mapping.executable.then_some(mapping.range))
                    .collect(),
                path,
                device_major,
                device_minor,
                inode,
            })
        })
        .collect()
}

/// Enumerate the file-backed ELF images mapped in this process, ordered by
/// `(path, device, inode, load instance)`.
pub fn loaded_images() -> std::io::Result<Vec<LoadedImage>> {
    let maps = std::fs::read_to_string("/proc/self/maps")?;
    let identities = loaded_elf_identities();
    Ok(linux_images(&maps)
        .into_iter()
        .map(|image| {
            let identity = identities.iter().find(|identity| {
                identity.mapped_ranges.iter().any(|loaded| {
                    image
                        .mapped_ranges
                        .iter()
                        .any(|mapped| loaded.start < mapped.end && mapped.start < loaded.end)
                })
            });
            LoadedImage {
                format: LoadedImageFormat::Elf,
                header_address: 0,
                image_size: 0,
                slide: 0,
                path: Some(image.path),
                mapped_ranges: image.mapped_ranges,
                executable_ranges: image.executable_ranges,
                elf_load_bias: identity.map(|identity| identity.load_bias),
                build_id: identity.map(|identity| identity.build_id.clone()),
                backing_file: Some(LoadedImageBackingFile {
                    device_major: image.device_major,
                    device_minor: image.device_minor,
                    inode: image.inode,
                }),
            }
        })
        .collect())
}

/// Reopen the file behind `image`, refusing one that no longer has the device
/// and inode recorded in `/proc/self/maps`.
pub fn open_loaded_image_file(image: &LoadedImage) -> Option<std::fs::File> {
    use std::os::unix::fs::MetadataExt as _;

    let file = std::fs::File::open(image.path.as_deref()?).ok()?;
    let Some(expected) = &image.backing_file else {
        return Some(file);
    };
    let metadata = file.metadata().ok()?;
    if u64::from(libc::major(metadata.dev())) != expected.device_major
        || u64::from(libc::minor(metadata.dev())) != expected.device_minor
        || metadata.ino().to_string() != expected.inode
    {
        // The pathname no longer names the object in /proc/self/maps.
        return None;
    }
    Some(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_path_preserves_spaces_and_deleted_suffix() {
        let line = "1000-2000 r-xp 00000000 08:01 42 /tmp/a file (deleted)";
        let (_, rest) = next_maps_field(line).unwrap();
        let (_, rest) = next_maps_field(rest).unwrap();
        let (_, rest) = next_maps_field(rest).unwrap();
        let (_, rest) = next_maps_field(rest).unwrap();
        let (_, rest) = next_maps_field(rest).unwrap();
        assert_eq!(rest.trim_start(), "/tmp/a file (deleted)");
        assert!(rest.trim_start().ends_with(" (deleted)"));
    }

    /// Pins the grouping the probe relied on before this moved (#974):
    /// mappings of one file at one load instance form one image, anonymous
    /// and deleted mappings are skipped, and only `x` mappings are executable.
    #[test]
    #[allow(clippy::single_range_in_vec_init)] // Lists of ranges, some of one.
    fn maps_lines_group_into_images_by_file_and_load_instance() {
        let maps = "\
7f0000002000-7f0000003000 r--p 00002000 08:01 42 /lib/a b.so
7f0000000000-7f0000001000 r-xp 00000000 08:01 42 /lib/a b.so
7f0000010000-7f0000011000 r-xp 00000000 08:01 42 /lib/a b.so
7f0000020000-7f0000021000 rw-p 00000000 00:00 0
7f0000030000-7f0000031000 r-xp 00000000 08:01 43 /lib/gone.so (deleted)
7f0000040000-7f0000041000 r-xp 00000000 fd:0a 44 /usr/bin/app
not a maps line
";
        let images = linux_images(maps);
        let summary: Vec<_> = images
            .iter()
            .map(|image| {
                (
                    image.path.as_str(),
                    image.mapped_ranges.clone(),
                    image.executable_ranges.clone(),
                    image.device_major,
                    image.device_minor,
                    image.inode.as_str(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    "/lib/a b.so",
                    vec![0x7f00_0000_0000..0x7f00_0000_1000, 0x7f00_0000_2000..0x7f00_0000_3000],
                    vec![0x7f00_0000_0000..0x7f00_0000_1000],
                    8,
                    1,
                    "42",
                ),
                (
                    "/lib/a b.so",
                    vec![0x7f00_0001_0000..0x7f00_0001_1000],
                    vec![0x7f00_0001_0000..0x7f00_0001_1000],
                    8,
                    1,
                    "42",
                ),
                (
                    "/usr/bin/app",
                    vec![0x7f00_0004_0000..0x7f00_0004_1000],
                    vec![0x7f00_0004_0000..0x7f00_0004_1000],
                    0xfd,
                    0x0a,
                    "44",
                ),
            ]
        );
    }

    #[test]
    fn gnu_note_parser_extracts_the_build_id() {
        let mut note = Vec::new();
        note.extend_from_slice(&4u32.to_ne_bytes());
        note.extend_from_slice(&4u32.to_ne_bytes());
        note.extend_from_slice(&3u32.to_ne_bytes());
        note.extend_from_slice(b"GNU\0");
        note.extend_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd]);
        assert_eq!(
            super::super::gnu_build_id_from_notes(&note),
            Some(&[0xaa, 0xbb, 0xcc, 0xdd][..])
        );
    }

    #[test]
    fn a_path_that_now_names_another_file_is_not_reopened() {
        let exe = std::env::current_exe().expect("current exe");
        let image = LoadedImage {
            format: LoadedImageFormat::Elf,
            header_address: 0,
            image_size: 0,
            slide: 0,
            path: Some(exe.to_string_lossy().into_owned()),
            mapped_ranges: Vec::new(),
            executable_ranges: Vec::new(),
            elf_load_bias: None,
            build_id: None,
            backing_file: Some(LoadedImageBackingFile {
                device_major: u64::MAX,
                device_minor: 0,
                inode: "0".into(),
            }),
        };
        assert!(open_loaded_image_file(&image).is_none());
    }
}
