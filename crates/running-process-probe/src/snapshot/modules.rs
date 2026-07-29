//! Loaded-module inventory and in-memory PE section lookup (#635).
//!
//! Unwinding a captured stack needs, for every loaded module, its image base
//! and the address ranges of specific sections — on Windows, `.pdata` and
//! `.xdata` carry the unwind tables. This module supplies that inventory.
//!
//! # Why parse the mapped image rather than the file
//!
//! The module is already mapped into this process, so its headers are directly
//! readable and no file I/O is needed. That matters because this inventory is
//! built to interpret captures taken while threads were suspended: touching
//! the filesystem here would make the capture path depend on disk
//! availability, and a module can be deleted or replaced on disk while still
//! mapped.
//!
//! # What this deliberately does not do
//!
//! No unwinding, and no symbolization. This is the address bookkeeping an
//! unwinder consumes, split out so it can be verified on its own — the ranges
//! it reports are checkable against known function addresses without any
//! unwinder existing yet.

#![allow(unsafe_code)] // Module enumeration and header reads are FFI/raw-pointer work.

use std::ops::Range;

#[cfg(windows)]
use std::io;
#[cfg(windows)]
use winapi::shared::minwindef::{DWORD, HMODULE};
#[cfg(windows)]
use winapi::um::processthreadsapi::GetCurrentProcess;
#[cfg(windows)]
use winapi::um::psapi::{
    EnumProcessModules, GetModuleFileNameExW, GetModuleInformation, MODULEINFO,
};
#[cfg(windows)]
use winapi::um::winnt::{
    IMAGE_DOS_HEADER, IMAGE_DOS_SIGNATURE, IMAGE_NT_HEADERS64, IMAGE_NT_SIGNATURE,
    IMAGE_SECTION_HEADER,
};

/// One section of a mapped module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    /// Section name as written in the PE header, e.g. `.text`.
    ///
    /// PE names are 8 bytes and are NOT NUL-terminated when exactly 8 long, so
    /// this is the trimmed form rather than a raw C string.
    pub name: String,
    /// Address range of the section as mapped in this process.
    pub range: Range<u64>,
}

/// A module loaded in this process.
#[derive(Clone, Debug)]
pub struct LoadedModule {
    /// Base address the module is mapped at.
    pub base: u64,
    /// Total mapped size.
    pub size: u64,
    /// Actual mapped address ranges when they differ from `base..base+size`.
    pub(crate) mapped_ranges: Vec<Range<u64>>,
    /// Full path of the module on disk, when the OS could report it.
    ///
    /// Needed downstream to find the symbol file, which lives beside the
    /// binary. `None` rather than a guess when the query fails: a wrong path
    /// would load a *different* build's symbols and produce confidently wrong
    /// function names.
    pub path: Option<String>,
    /// Sections parsed from the mapped headers.
    pub sections: Vec<Section>,
}

impl LoadedModule {
    /// Address range covered by the whole module.
    pub fn range(&self) -> Range<u64> {
        match (self.mapped_ranges.first(), self.mapped_ranges.last()) {
            (Some(first), Some(last)) => first.start..last.end,
            _ => self.base..self.base + self.size,
        }
    }

    /// Whether `address` falls inside this module.
    pub fn contains(&self, address: u64) -> bool {
        if self.mapped_ranges.is_empty() {
            self.range().contains(&address)
        } else {
            self.mapped_ranges
                .iter()
                .any(|range| range.contains(&address))
        }
    }

    /// Look up a section by name, e.g. `.text` or `.pdata`.
    pub fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }
}

/// Read the section table out of a module already mapped at `base`.
///
/// # Safety
///
/// `base` must be the base address of a PE image currently mapped into this
/// process. Callers get that from [`enumerate_modules`], which obtains it from
/// the OS.
#[cfg(windows)]
unsafe fn read_sections(base: u64) -> Option<Vec<Section>> {
    let dos = base as *const IMAGE_DOS_HEADER;
    if (*dos).e_magic != IMAGE_DOS_SIGNATURE {
        return None;
    }

    // e_lfanew is a signed offset from the image base to the NT headers.
    let lfanew = (*dos).e_lfanew;
    if lfanew < 0 {
        return None;
    }
    let nt = (base + lfanew as u64) as *const IMAGE_NT_HEADERS64;
    if (*nt).Signature != IMAGE_NT_SIGNATURE {
        return None;
    }

    let section_count = (*nt).FileHeader.NumberOfSections as usize;
    // The section table follows the optional header, whose size is declared
    // rather than fixed — using size_of::<IMAGE_OPTIONAL_HEADER64>() would
    // silently misread images with a different optional-header size.
    let opt_size = (*nt).FileHeader.SizeOfOptionalHeader as u64;
    let opt_start = base + lfanew as u64 + 4 /* Signature */ + 20 /* FileHeader */;
    let table = (opt_start + opt_size) as *const IMAGE_SECTION_HEADER;

    let mut sections = Vec::with_capacity(section_count);
    for i in 0..section_count {
        let header = &*table.add(i);

        // PE section names occupy exactly 8 bytes and are only NUL-terminated
        // when shorter, so take bytes up to the first NUL rather than assuming
        // one exists.
        let raw = &header.Name;
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        let name = String::from_utf8_lossy(&raw[..end]).into_owned();

        let start = base + u64::from(header.VirtualAddress);
        // VirtualSize is the in-memory size; SizeOfRawData is the on-disk one
        // and can differ (BSS-like sections have raw size 0).
        let size = u64::from(unsafe { *header.Misc.VirtualSize() });

        sections.push(Section {
            name,
            range: start..start + size,
        });
    }
    Some(sections)
}

/// Enumerate every module mapped into this process.
#[cfg(windows)]
pub fn enumerate_modules() -> io::Result<Vec<LoadedModule>> {
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

    let mut modules = Vec::with_capacity(usable);
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
            // Unloaded between enumeration and query. Skip rather than fail
            // the whole inventory.
            continue;
        }

        let base = info.lpBaseOfDll as u64;
        let sections = match unsafe { read_sections(base) } {
            Some(s) => s,
            None => continue,
        };

        modules.push(LoadedModule {
            base,
            size: u64::from(info.SizeOfImage),
            mapped_ranges: Vec::new(),
            path: unsafe { module_path(process, handle) },
            sections,
        });
    }

    Ok(modules)
}

/// Full path of a loaded module, or `None` if the OS would not say.
///
/// # Safety
///
/// `handle` must be a module handle obtained from `process`.
#[cfg(windows)]
unsafe fn module_path(process: winapi::um::winnt::HANDLE, handle: HMODULE) -> Option<String> {
    let mut buffer = [0u16; 32768];
    let len = GetModuleFileNameExW(process, handle, buffer.as_mut_ptr(), buffer.len() as DWORD);
    if len == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buffer[..len as usize]))
}

/// Find the module containing `address`.
pub fn module_for_address(modules: &[LoadedModule], address: u64) -> Option<&LoadedModule> {
    modules.iter().find(|m| m.contains(address))
}

#[cfg(target_os = "linux")]
fn next_maps_field(input: &str) -> Option<(&str, &str)> {
    let input = input.trim_start();
    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    (!input.is_empty()).then_some((&input[..end], &input[end..]))
}

#[cfg(target_os = "linux")]
fn linux_images() -> std::io::Result<Vec<(Vec<Range<u64>>, u64, String)>> {
    use std::collections::BTreeMap;

    // (path, device, inode, load instance) -> individual mapped ranges.
    let mut images: BTreeMap<(String, String, String, u64), Vec<Range<u64>>> = BTreeMap::new();
    for line in std::fs::read_to_string("/proc/self/maps")?.lines() {
        let Some((range, rest)) = next_maps_field(line) else {
            continue;
        };
        let Some((_perms, rest)) = next_maps_field(rest) else {
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
            .push(start..end);
    }

    Ok(images
        .into_iter()
        .map(|((path, _dev, _inode, load_bias), mut ranges)| {
            ranges.sort_by_key(|range| range.start);
            (ranges, load_bias, path)
        })
        .collect())
}

#[cfg(target_os = "linux")]
/// Enumerate ELF images mapped in the current Linux process.
pub fn enumerate_modules() -> std::io::Result<Vec<LoadedModule>> {
    use object::{Object, ObjectKind, ObjectSection};

    let mut modules = Vec::new();
    for (mapped_ranges, load_bias, path) in linux_images()? {
        let Some(mapped_start) = mapped_ranges.first().map(|range| range.start) else {
            continue;
        };
        let Some(mapped_end) = mapped_ranges.last().map(|range| range.end) else {
            continue;
        };
        let Ok(data) = std::fs::read(&path) else {
            // A deleted/replaced mapping remains valid for raw capture but
            // cannot safely provide unwind metadata from disk. Leave it out
            // rather than attribute it to a different build.
            continue;
        };
        let Ok(file) = object::File::parse(data.as_slice()) else {
            continue;
        };
        let base = if file.kind() == ObjectKind::Executable {
            0
        } else {
            load_bias
        };
        let sections = file
            .sections()
            .filter_map(|section| {
                let name = section.name().ok()?.to_owned();
                let start = base.checked_add(section.address())?;
                let end = start.checked_add(section.size())?;
                Some(Section {
                    name,
                    range: start..end,
                })
            })
            .collect();

        modules.push(LoadedModule {
            base,
            size: mapped_end.saturating_sub(mapped_start),
            mapped_ranges,
            path: Some(path),
            sections,
        });
    }
    modules.sort_by_key(|module| module.base);
    Ok(modules)
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn _dyld_image_count() -> u32;
    fn _dyld_get_image_header(image_index: u32) -> *const libc::c_void;
    fn _dyld_get_image_vmaddr_slide(image_index: u32) -> isize;
    fn _dyld_get_image_name(image_index: u32) -> *const libc::c_char;
}

#[cfg(target_os = "macos")]
fn add_slide(address: u64, slide: isize) -> Option<u64> {
    if slide >= 0 {
        address.checked_add(slide as u64)
    } else {
        address.checked_sub(slide.unsigned_abs() as u64)
    }
}

#[cfg(target_os = "macos")]
/// Enumerate Mach-O images loaded by dyld in the current macOS process.
pub fn enumerate_modules() -> std::io::Result<Vec<LoadedModule>> {
    use object::{Object, ObjectSection, ObjectSegment};
    use std::ffi::CStr;

    let count = unsafe { _dyld_image_count() };
    let mut modules = Vec::with_capacity(count as usize);
    for index in 0..count {
        let name = unsafe { _dyld_get_image_name(index) };
        let header = unsafe { _dyld_get_image_header(index) };
        if name.is_null() || header.is_null() {
            continue;
        }
        let path = unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned();
        let Ok(data) = std::fs::read(&path) else {
            // Some system images live only in the shared dyld cache. Their
            // raw frames remain unattributed rather than being paired with
            // metadata read from a different file.
            continue;
        };
        let Ok(file) = object::File::parse(data.as_slice()) else {
            continue;
        };
        let slide = unsafe { _dyld_get_image_vmaddr_slide(index) };
        let base_svma = file.relative_address_base();
        let base = add_slide(base_svma, slide).unwrap_or(header as u64);

        let mut mapped_start = u64::MAX;
        let mut mapped_end = 0u64;
        for segment in file.segments() {
            if segment.name().ok().flatten() == Some("__PAGEZERO") {
                continue;
            }
            let Some(start) = add_slide(segment.address(), slide) else {
                continue;
            };
            let Some(end) = start.checked_add(segment.size()) else {
                continue;
            };
            mapped_start = mapped_start.min(start);
            mapped_end = mapped_end.max(end);
        }
        if mapped_start == u64::MAX || mapped_end <= base {
            continue;
        }

        let sections = file
            .sections()
            .filter_map(|section| {
                let name = section.name().ok()?.to_owned();
                let start = add_slide(section.address(), slide)?;
                let end = start.checked_add(section.size())?;
                Some(Section {
                    name,
                    range: start..end,
                })
            })
            .collect();
        let _ = mapped_start;
        modules.push(LoadedModule {
            base,
            size: mapped_end - base,
            mapped_ranges: Vec::new(),
            path: Some(path),
            sections,
        });
    }
    modules.sort_by_key(|module| module.base);
    Ok(modules)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// A distinctive function whose address is used to locate `.text` below.
    #[inline(never)]
    fn landmark() -> u64 {
        // The black_box keeps this from being optimized into nothing, which
        // would make its address meaningless.
        std::hint::black_box(0xD1A6_0057_u64)
    }

    #[test]
    fn enumeration_finds_at_least_the_executable_and_some_dlls() {
        let modules = enumerate_modules().expect("enumerate");
        assert!(
            modules.len() >= 2,
            "expected the exe plus at least one DLL, got {}",
            modules.len()
        );
    }

    #[test]
    fn every_module_reports_a_nonempty_range_and_sections() {
        for m in enumerate_modules().expect("enumerate") {
            assert!(m.base != 0, "module with null base");
            assert!(m.size > 0, "module with zero size at {:#x}", m.base);
            assert!(
                !m.sections.is_empty(),
                "module at {:#x} parsed no sections",
                m.base
            );
        }
    }

    /// The decisive check: a real function's address must land inside the
    /// `.text` range of the module reporting it.
    ///
    /// This verifies the section arithmetic end-to-end without any unwinder —
    /// a wrong base, a wrong optional-header size, or a misread VirtualAddress
    /// all fail here.
    #[test]
    fn text_section_contains_a_known_function_address() {
        let addr = (landmark as fn() -> u64) as usize as u64;
        let modules = enumerate_modules().expect("enumerate");

        let owner = module_for_address(&modules, addr)
            .unwrap_or_else(|| panic!("no module contains {addr:#x}"));

        let text = owner
            .section(".text")
            .unwrap_or_else(|| panic!("module at {:#x} has no .text", owner.base));

        assert!(
            text.range.contains(&addr),
            "function at {addr:#x} is outside its module's .text ({:#x}..{:#x})",
            text.range.start,
            text.range.end
        );
        // Sanity: the landmark still evaluates, so it was not optimized away.
        assert_eq!(landmark(), 0xD1A6_0057_u64);
    }

    #[test]
    fn module_lookup_rejects_an_address_outside_every_module() {
        let modules = enumerate_modules().expect("enumerate");
        // A deliberately implausible user-mode address.
        assert!(module_for_address(&modules, 0x1).is_none());
    }

    /// Unwinding needs `.pdata`; confirm the inventory actually surfaces it for
    /// the module holding our own code.
    #[test]
    fn own_module_exposes_unwind_sections() {
        let addr = (landmark as fn() -> u64) as usize as u64;
        let modules = enumerate_modules().expect("enumerate");
        let owner = module_for_address(&modules, addr).expect("owning module");

        assert!(
            owner.section(".pdata").is_some(),
            "x86_64 PE modules carry .pdata unwind tables; sections found: {:?}",
            owner.sections.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn sections_do_not_extend_past_their_module() {
        for m in enumerate_modules().expect("enumerate") {
            let module_end = m.base + m.size;
            for s in &m.sections {
                assert!(
                    s.range.start >= m.base && s.range.start <= module_end,
                    "section {} at {:#x} lies outside module {:#x}..{:#x}",
                    s.name,
                    s.range.start,
                    m.base,
                    module_end
                );
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
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

    #[test]
    fn elf_load_bias_does_not_expand_mapped_coverage_to_zero() {
        let module = LoadedModule {
            base: 0,
            size: 0x2000,
            mapped_ranges: vec![0x400000..0x401000, 0x402000..0x403000],
            path: Some("/tmp/non-pie".into()),
            sections: Vec::new(),
        };
        assert!(module.contains(0x400100));
        assert!(module.contains(0x402100));
        assert!(!module.contains(0x401100));
        assert!(!module.contains(1));
    }
}
