//! Loaded-image inventory for the current Windows process, from PSAPI (#974).

use std::io;

use winapi::shared::minwindef::{DWORD, HMODULE};
use winapi::um::processthreadsapi::GetCurrentProcess;
use winapi::um::psapi::{EnumProcessModules, GetModuleFileNameExW, GetModuleInformation, MODULEINFO};

use crate::platform::process::{LoadedImage, LoadedImageFormat};

/// Enumerate every module mapped into this process, in PSAPI's order.
///
/// A module that unloads between enumeration and query is skipped rather than
/// failing the whole inventory.
pub fn loaded_images() -> io::Result<Vec<LoadedImage>> {
    let process = unsafe { GetCurrentProcess() };

    // Two-pass: ask how many bytes are needed, then fetch. A single fixed-size
    // pass would silently truncate in a process with many DLLs loaded.
    let mut needed: DWORD = 0;
    let ok = unsafe { EnumProcessModules(process, std::ptr::null_mut(), 0, &mut needed) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }

    let count = needed as usize / std::mem::size_of::<HMODULE>();
    let mut handles: Vec<HMODULE> = vec![std::ptr::null_mut(); count];
    let mut needed2: DWORD = 0;
    let ok = unsafe {
        EnumProcessModules(
            process,
            handles.as_mut_ptr(),
            (handles.len() * std::mem::size_of::<HMODULE>()) as DWORD,
            &mut needed2,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // A module can load between the two calls; honor the smaller count.
    let usable = (needed2 as usize / std::mem::size_of::<HMODULE>()).min(handles.len());

    let mut images = Vec::with_capacity(usable);
    for handle in handles.into_iter().take(usable) {
        let mut info: MODULEINFO = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            GetModuleInformation(
                process,
                handle,
                &mut info,
                std::mem::size_of::<MODULEINFO>() as DWORD,
            )
        };
        if ok == 0 {
            continue;
        }
        images.push(LoadedImage {
            format: LoadedImageFormat::Pe,
            header_address: info.lpBaseOfDll as u64,
            image_size: u64::from(info.SizeOfImage),
            slide: 0,
            path: unsafe { module_path(process, handle) },
            mapped_ranges: Vec::new(),
            executable_ranges: Vec::new(),
            elf_load_bias: None,
            build_id: None,
            backing_file: None,
        });
    }
    Ok(images)
}

/// Full path of a loaded module, or `None` if the OS would not say.
///
/// # Safety
///
/// `handle` must be a module handle obtained from `process`.
unsafe fn module_path(process: winapi::um::winnt::HANDLE, handle: HMODULE) -> Option<String> {
    let mut buffer = [0u16; 32768];
    let len = unsafe {
        GetModuleFileNameExW(process, handle, buffer.as_mut_ptr(), buffer.len() as DWORD)
    };
    if len == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buffer[..len as usize]))
}

/// Reopen the file PSAPI named for `image`.
pub fn open_loaded_image_file(image: &LoadedImage) -> Option<std::fs::File> {
    std::fs::File::open(image.path.as_deref()?).ok()
}
