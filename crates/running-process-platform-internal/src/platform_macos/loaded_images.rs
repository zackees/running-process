//! Loaded-image inventory for the current macOS process, from dyld (#974).

use crate::platform::process::{LoadedImage, LoadedImageFormat};

unsafe extern "C" {
    fn _dyld_image_count() -> u32;
    fn _dyld_get_image_header(image_index: u32) -> *const libc::c_void;
    fn _dyld_get_image_vmaddr_slide(image_index: u32) -> isize;
    fn _dyld_get_image_name(image_index: u32) -> *const libc::c_char;
}

/// Enumerate the Mach-O images dyld reports, in dyld's index order.
///
/// An index whose name or header dyld cannot supply is skipped.
pub fn loaded_images() -> std::io::Result<Vec<LoadedImage>> {
    use std::ffi::CStr;

    let count = unsafe { _dyld_image_count() };
    let mut images = Vec::with_capacity(count as usize);
    for index in 0..count {
        let name = unsafe { _dyld_get_image_name(index) };
        let header = unsafe { _dyld_get_image_header(index) };
        if name.is_null() || header.is_null() {
            continue;
        }
        let path = unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned();
        let slide = unsafe { _dyld_get_image_vmaddr_slide(index) };
        images.push(LoadedImage {
            format: LoadedImageFormat::MachO,
            header_address: header as u64,
            image_size: 0,
            slide: slide as i64,
            path: Some(path),
            mapped_ranges: Vec::new(),
            executable_ranges: Vec::new(),
            elf_load_bias: None,
            build_id: None,
            backing_file: None,
        });
    }
    Ok(images)
}

/// Reopen the file dyld named for `image`.
///
/// dyld records no file identity to re-check, so the caller must still match
/// the file against the loaded image (its `LC_UUID`).
pub fn open_loaded_image_file(image: &LoadedImage) -> Option<std::fs::File> {
    std::fs::File::open(image.path.as_deref()?).ok()
}
