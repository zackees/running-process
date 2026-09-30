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
//! # Where the host ends and the format begins (#974)
//!
//! Asking the loader which images are mapped is host mechanics, and lives in
//! the platform facade (`platform::process::loaded_images`). Everything here
//! is object-format work — PE headers, Mach-O load commands, ELF/Mach-O
//! section tables — selected by the format the loader reports, not by the
//! host this was compiled for.
//!
//! # What this deliberately does not do
//!
//! No unwinding, and no symbolization. This is the address bookkeeping an
//! unwinder consumes, split out so it can be verified on its own — the ranges
//! it reports are checkable against known function addresses without any
//! unwinder existing yet.

#![allow(unsafe_code)] // Header reads out of mapped images are raw-pointer work.

use std::io;
use std::ops::Range;

use running_process_platform_internal::platform::process::{
    loaded_images, open_loaded_image_file, LoadedImage, LoadedImageFormat,
};

const MAX_MODULE_IMAGE_BYTES: u64 = 512 * 1024 * 1024;

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `IMAGE_DOS_SIGNATURE` ("MZ").
const PE_DOS_SIGNATURE: u16 = 0x5A4D;
/// `IMAGE_NT_SIGNATURE` ("PE\0\0").
const PE_NT_SIGNATURE: u32 = 0x0000_4550;
/// Offset of `e_lfanew` in `IMAGE_DOS_HEADER`.
const PE_LFANEW_OFFSET: u64 = 0x3C;
/// `size_of::<IMAGE_NT_HEADERS64>()`: signature, file header, PE32+ optional header.
const PE_NT_HEADERS64_SIZE: usize = 4 + 20 + 240;
/// Offset of `DataDirectory` inside `IMAGE_OPTIONAL_HEADER64`.
const PE_OPTIONAL_HEADER64_DATA_DIRECTORY: u64 = 112;
/// `size_of::<IMAGE_SECTION_HEADER>()`.
const PE_SECTION_HEADER_SIZE: u64 = 40;

unsafe fn read_u16(address: u64) -> u16 {
    unsafe { (address as *const u16).read_unaligned() }
}

unsafe fn read_u32(address: u64) -> u32 {
    unsafe { (address as *const u32).read_unaligned() }
}

/// # Safety
///
/// `base` must be the base of a PE image mapped into this process and
/// `image_size` its mapped size.
unsafe fn loaded_pe_debug_info(base: u64, image_size: u64) -> Option<(String, String)> {
    const DEBUG_DIRECTORY_INDEX: u64 = 6;
    const IMAGE_DEBUG_TYPE_CODEVIEW: u32 = 2;
    const DEBUG_DIRECTORY_SIZE: usize = 28;

    if unsafe { read_u16(base) } != PE_DOS_SIGNATURE {
        return None;
    }
    let e_lfanew = unsafe { read_u32(base + PE_LFANEW_OFFSET) } as i32;
    let nt_offset = usize::try_from(e_lfanew).ok()?;
    if nt_offset.checked_add(PE_NT_HEADERS64_SIZE)? > usize::try_from(image_size).ok()? {
        return None;
    }
    let nt_address = (base as usize).checked_add(nt_offset)? as u64;
    if unsafe { read_u32(nt_address) } != PE_NT_SIGNATURE {
        return None;
    }
    let directory_entry =
        nt_address + 4 + 20 + PE_OPTIONAL_HEADER64_DATA_DIRECTORY + DEBUG_DIRECTORY_INDEX * 8;
    let directory_start = u64::from(unsafe { read_u32(directory_entry) });
    let directory_size = usize::try_from(unsafe { read_u32(directory_entry + 4) }).ok()?;
    if directory_start
        .checked_add(directory_size as u64)?
        .gt(&image_size)
    {
        return None;
    }
    let directory_address = base.checked_add(directory_start)?;
    let bytes =
        unsafe { std::slice::from_raw_parts(directory_address as *const u8, directory_size) };
    for entry in bytes.chunks_exact(DEBUG_DIRECTORY_SIZE) {
        let kind = u32::from_le_bytes(entry[12..16].try_into().ok()?);
        if kind != IMAGE_DEBUG_TYPE_CODEVIEW {
            continue;
        }
        let size = usize::try_from(u32::from_le_bytes(entry[16..20].try_into().ok()?)).ok()?;
        let rva = u64::from(u32::from_le_bytes(entry[20..24].try_into().ok()?));
        if size < 24 || rva.checked_add(size as u64)?.gt(&image_size) {
            continue;
        }
        let record_address = base.checked_add(rva)?;
        let record = unsafe { std::slice::from_raw_parts(record_address as *const u8, size) };
        if record.get(..4) != Some(b"RSDS") {
            continue;
        }
        let mut guid: [u8; 16] = record.get(4..20)?.try_into().ok()?;
        guid[0..4].reverse();
        guid[4..6].reverse();
        guid[6..8].reverse();
        let age = u32::from_le_bytes(record.get(20..24)?.try_into().ok()?);
        let path_bytes = record.get(24..)?;
        let path_end = path_bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(path_bytes.len());
        let recorded = String::from_utf8_lossy(&path_bytes[..path_end]);
        let pdb_name = recorded
            .rsplit(['/', '\\'])
            .find(|part| !part.is_empty())?
            .to_owned();
        if pdb_name == "."
            || pdb_name == ".."
            || pdb_name.chars().any(|character| {
                matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
            })
        {
            return None;
        }
        return Some((format!("pdb:{}-{age}", hex_bytes(&guid)), pdb_name));
    }
    None
}

/// # Safety
///
/// `header` must point at a Mach-O `mach_header` mapped into this process.
unsafe fn loaded_macho_uuid(header: *const u8) -> Option<[u8; 16]> {
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const LC_UUID: u32 = 0x1b;
    const MACH_HEADER_64_SIZE: usize = 32;
    const MAX_LOAD_COMMAND_BYTES: usize = 1024 * 1024;
    const MAX_LOAD_COMMANDS: u32 = 4096;

    let magic = unsafe { (header.cast::<u32>()).read_unaligned() };
    if magic != MH_MAGIC_64 {
        return None;
    }
    let ncmds = unsafe { header.add(16).cast::<u32>().read_unaligned() };
    let sizeofcmds =
        usize::try_from(unsafe { header.add(20).cast::<u32>().read_unaligned() }).ok()?;
    if ncmds > MAX_LOAD_COMMANDS || sizeofcmds > MAX_LOAD_COMMAND_BYTES {
        return None;
    }
    let commands =
        unsafe { std::slice::from_raw_parts(header.add(MACH_HEADER_64_SIZE), sizeofcmds) };
    let mut offset = 0usize;
    for _ in 0..ncmds {
        let prefix = commands.get(offset..offset.checked_add(8)?)?;
        let command = u32::from_le_bytes(prefix[0..4].try_into().ok()?);
        let size = usize::try_from(u32::from_le_bytes(prefix[4..8].try_into().ok()?)).ok()?;
        if size < 8 || offset.checked_add(size)? > commands.len() {
            return None;
        }
        if command == LC_UUID && size >= 24 {
            return commands.get(offset + 8..offset + 24)?.try_into().ok();
        }
        offset += size;
    }
    None
}

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
    /// Mapped ranges whose OS protection permits instruction execution.
    ///
    /// Read only by the x86_64 Linux/macOS unwinder, so unused elsewhere.
    #[allow(dead_code)]
    pub(crate) executable_ranges: Vec<Range<u64>>,
    /// Full path of the module on disk, when the OS could report it.
    ///
    /// Needed downstream to find the symbol file, which lives beside the
    /// binary. `None` rather than a guess when the query fails: a wrong path
    /// would load a *different* build's symbols and produce confidently wrong
    /// function names.
    pub path: Option<String>,
    /// Build identity observed while this module inventory was taken.
    ///
    /// This is read from the loaded image (or, on Linux, from the exact
    /// device/inode still backing the mapping), so later path replacement
    /// cannot change which symbols the capture expects.
    pub debug_id: Option<String>,
    /// Sanitized native symbol filename captured from the loaded image.
    pub debug_file: Option<String>,
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

    /// Whether `address` falls inside an executable mapping for this module.
    #[allow(dead_code)] // See `executable_ranges`.
    pub(crate) fn contains_executable(&self, address: u64) -> bool {
        self.executable_ranges
            .iter()
            .any(|range| range.contains(&address))
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
unsafe fn read_sections(base: u64) -> Option<Vec<Section>> {
    if unsafe { read_u16(base) } != PE_DOS_SIGNATURE {
        return None;
    }

    // e_lfanew is a signed offset from the image base to the NT headers.
    let lfanew = unsafe { read_u32(base + PE_LFANEW_OFFSET) } as i32;
    if lfanew < 0 {
        return None;
    }
    let nt = base + lfanew as u64;
    if unsafe { read_u32(nt) } != PE_NT_SIGNATURE {
        return None;
    }

    let section_count = unsafe { read_u16(nt + 4 + 2) } as usize;
    // The section table follows the optional header, whose size is declared
    // rather than fixed — using size_of::<IMAGE_OPTIONAL_HEADER64>() would
    // silently misread images with a different optional-header size.
    let opt_size = unsafe { read_u16(nt + 4 + 16) } as u64;
    let opt_start = nt + 4 /* Signature */ + 20 /* FileHeader */;
    let table = opt_start + opt_size;

    let mut sections = Vec::with_capacity(section_count);
    for i in 0..section_count as u64 {
        let header = table + i * PE_SECTION_HEADER_SIZE;

        // PE section names occupy exactly 8 bytes and are only NUL-terminated
        // when shorter, so take bytes up to the first NUL rather than assuming
        // one exists.
        let raw = unsafe { std::slice::from_raw_parts(header as *const u8, 8) };
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        let name = String::from_utf8_lossy(&raw[..end]).into_owned();

        let start = base + u64::from(unsafe { read_u32(header + 12) });
        // VirtualSize is the in-memory size; SizeOfRawData is the on-disk one
        // and can differ (BSS-like sections have raw size 0).
        let size = u64::from(unsafe { read_u32(header + 8) });

        sections.push(Section {
            name,
            range: start..start + size,
        });
    }
    Some(sections)
}

/// Enumerate every module mapped into this process.
pub fn enumerate_modules() -> io::Result<Vec<LoadedModule>> {
    let mut modules = Vec::new();
    let mut sort = false;
    for image in loaded_images()? {
        let module = match image.format {
            LoadedImageFormat::Pe => pe_module(image.header_address, image.image_size, image.path),
            LoadedImageFormat::Elf => {
                sort = true;
                elf_module(image)
            }
            LoadedImageFormat::MachO => {
                sort = true;
                macho_module(image)
            }
            _ => None,
        };
        modules.extend(module);
    }
    if sort {
        modules.sort_by_key(|module| module.base);
    }
    Ok(modules)
}

fn pe_module(base: u64, image_size: u64, path: Option<String>) -> Option<LoadedModule> {
    let sections = unsafe { read_sections(base) }?;
    let (debug_id, debug_file) = unsafe {
        loaded_pe_debug_info(base, image_size)
            .map(|(identity, file)| (Some(identity), Some(file)))
            .unwrap_or((None, None))
    };
    Some(LoadedModule {
        base,
        size: image_size,
        mapped_ranges: Vec::new(),
        executable_ranges: Vec::new(),
        path,
        debug_id,
        debug_file,
        sections,
    })
}

/// Read the whole file behind `image`, bounded, or `None`.
fn read_image_file(image: &LoadedImage) -> Option<Vec<u8>> {
    use std::io::Read as _;

    let file_handle = open_loaded_image_file(image)?;
    // Refuse an oversized file before reading any of it: `/proc/self/maps`
    // lists every file-backed mapping, large data files included. A metadata
    // failure falls through to the bounded read below.
    if let Ok(metadata) = file_handle.metadata() {
        if metadata.len() > MAX_MODULE_IMAGE_BYTES {
            return None;
        }
    }
    let mut data = Vec::new();
    if file_handle
        .take(MAX_MODULE_IMAGE_BYTES + 1)
        .read_to_end(&mut data)
        .is_err()
        || data.len() as u64 > MAX_MODULE_IMAGE_BYTES
    {
        // A deleted/replaced mapping remains valid for raw capture but
        // cannot safely provide unwind metadata from disk. Leave it out
        // rather than attribute it to a different build.
        return None;
    }
    Some(data)
}

fn elf_module(image: LoadedImage) -> Option<LoadedModule> {
    use object::{Object, ObjectKind, ObjectSection};

    let mapped_start = image.mapped_ranges.first()?.start;
    let mapped_end = image.mapped_ranges.last()?.end;
    let data = read_image_file(&image)?;
    let file = object::File::parse(data.as_slice()).ok()?;
    let load_bias = image.elf_load_bias?;
    let debug_id = format!("elf:{}", hex_bytes(image.build_id.as_deref()?));
    let base = if file.kind() == ObjectKind::Executable {
        0
    } else {
        load_bias
    };
    let file_debug_id = file
        .build_id()
        .ok()
        .flatten()
        .map(|build_id| format!("elf:{}", hex_bytes(build_id)));
    if file_debug_id.as_deref() != Some(debug_id.as_str()) {
        // Device/inode stability is not enough: an in-place overwrite can
        // preserve both. Never consume section metadata unless the file
        // still carries the build-id observed in the mapped PT_NOTE.
        return None;
    }
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

    Some(LoadedModule {
        base,
        size: mapped_end.saturating_sub(mapped_start),
        mapped_ranges: image.mapped_ranges,
        executable_ranges: image.executable_ranges,
        path: image.path,
        debug_id: Some(debug_id),
        debug_file: None,
        sections,
    })
}

fn add_slide(address: u64, slide: i64) -> Option<u64> {
    if slide >= 0 {
        address.checked_add(slide as u64)
    } else {
        address.checked_sub(slide.unsigned_abs())
    }
}

fn macho_module(image: LoadedImage) -> Option<LoadedModule> {
    use object::{Object, ObjectSection, ObjectSegment};

    let header = image.header_address;
    let loaded_uuid = unsafe { loaded_macho_uuid(header as *const u8) }?;
    // Some system images live only in the shared dyld cache. Their raw frames
    // remain unattributed rather than being paired with metadata read from a
    // different file.
    let data = read_image_file(&image)?;
    let file = object::File::parse(data.as_slice()).ok()?;
    if file.mach_uuid().ok().flatten() != Some(loaded_uuid) {
        // The path can be replaced while dyld keeps the original image
        // mapped. Only use on-disk sections for the exact loaded UUID.
        return None;
    }
    let slide = image.slide;
    let debug_id = Some(format!("macho:{}", hex_bytes(&loaded_uuid)));
    let base_svma = file.relative_address_base();
    let base = add_slide(base_svma, slide).unwrap_or(header);

    let mut mapped_start = u64::MAX;
    let mut mapped_end = 0u64;
    let mut executable_ranges = Vec::new();
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
        if matches!(
            segment.flags(),
            object::SegmentFlags::MachO { initprot, .. }
                if initprot & object::macho::VM_PROT_EXECUTE != 0
        ) {
            executable_ranges.push(start..end);
        }
    }
    if mapped_start == u64::MAX || mapped_end <= base {
        return None;
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
    Some(LoadedModule {
        base,
        size: mapped_end - base,
        mapped_ranges: Vec::new(),
        executable_ranges,
        path: image.path,
        debug_id,
        debug_file: None,
        sections,
    })
}

/// Find the module containing `address`.
pub fn module_for_address(modules: &[LoadedModule], address: u64) -> Option<&LoadedModule> {
    modules.iter().find(|m| m.contains(address))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elf_load_bias_does_not_expand_mapped_coverage_to_zero() {
        let module = LoadedModule {
            base: 0,
            size: 0x2000,
            mapped_ranges: vec![0x400000..0x401000, 0x402000..0x403000],
            executable_ranges: std::iter::once(0x400000..0x401000).collect(),
            path: Some("/tmp/non-pie".into()),
            debug_id: None,
            debug_file: None,
            sections: Vec::new(),
        };
        assert!(module.contains(0x400100));
        assert!(module.contains(0x402100));
        assert!(!module.contains(0x401100));
        assert!(!module.contains(1));
        assert!(module.contains_executable(0x400100));
        assert!(!module.contains_executable(0x402100));
    }

    /// A minimal mapped PE32+ image: DOS header, NT headers, two sections and
    /// a CodeView debug directory, laid out the way the loader maps them.
    ///
    /// Before #974 this parsing compiled only on Windows, so nothing checked
    /// its offsets anywhere else; now it is format code on every host.
    fn synthetic_pe() -> Vec<u8> {
        const SIZE: usize = 0x1000;
        const NT: usize = 0x80;
        let mut image = vec![0u8; SIZE];
        image[0..2].copy_from_slice(&0x5A4Du16.to_le_bytes());
        image[0x3C..0x40].copy_from_slice(&(NT as u32).to_le_bytes());
        image[NT..NT + 4].copy_from_slice(b"PE\0\0");
        // FileHeader: NumberOfSections = 2, SizeOfOptionalHeader = 240.
        image[NT + 6..NT + 8].copy_from_slice(&2u16.to_le_bytes());
        image[NT + 20..NT + 22].copy_from_slice(&240u16.to_le_bytes());
        // DataDirectory[6] (debug): RVA 0x400, one 28-byte entry.
        let debug_dir = NT + 24 + 112 + 6 * 8;
        image[debug_dir..debug_dir + 4].copy_from_slice(&0x400u32.to_le_bytes());
        image[debug_dir + 4..debug_dir + 8].copy_from_slice(&28u32.to_le_bytes());
        // Section table after the declared optional header.
        let table = NT + 24 + 240;
        let sections: [(&[u8], u32, u32); 2] =
            [(b".text", 0x200, 0x100), (b".pdata\0\0", 0x300, 0x40)];
        for (index, (name, rva, size)) in sections.iter().enumerate() {
            let header = table + index * 40;
            image[header..header + name.len()].copy_from_slice(name);
            image[header + 8..header + 12].copy_from_slice(&size.to_le_bytes());
            image[header + 12..header + 16].copy_from_slice(&rva.to_le_bytes());
        }
        // IMAGE_DEBUG_DIRECTORY: Type CODEVIEW, SizeOfData, AddressOfRawData.
        let record = b"RSDS\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f\x10\x07\x00\x00\x00C:\\build\\probe.pdb\0";
        image[0x400 + 12..0x400 + 16].copy_from_slice(&2u32.to_le_bytes());
        image[0x400 + 16..0x400 + 20].copy_from_slice(&(record.len() as u32).to_le_bytes());
        image[0x400 + 20..0x400 + 24].copy_from_slice(&0x500u32.to_le_bytes());
        image[0x500..0x500 + record.len()].copy_from_slice(record);
        image
    }

    #[test]
    fn pe_headers_are_read_from_the_mapped_image_on_every_host() {
        let image = synthetic_pe();
        let base = image.as_ptr() as u64;
        let sections = unsafe { read_sections(base) }.expect("sections");
        assert_eq!(
            sections,
            vec![
                Section {
                    name: ".text".into(),
                    range: base + 0x200..base + 0x300,
                },
                Section {
                    name: ".pdata".into(),
                    range: base + 0x300..base + 0x340,
                },
            ]
        );
        let (identity, file) =
            unsafe { loaded_pe_debug_info(base, image.len() as u64) }.expect("codeview");
        assert_eq!(identity, "pdb:0403020106050807090a0b0c0d0e0f10-7");
        assert_eq!(file, "probe.pdb");

        let module = pe_module(
            base,
            image.len() as u64,
            Some("C:\\build\\probe.exe".into()),
        )
        .expect("module");
        assert_eq!(module.range(), base..base + image.len() as u64);
        assert_eq!(
            module.section(".pdata").map(|s| s.range.start),
            Some(base + 0x300)
        );
    }

    #[test]
    fn a_pe_image_without_its_signatures_is_rejected() {
        let mut image = synthetic_pe();
        image[0x80] = b'X';
        let base = image.as_ptr() as u64;
        assert!(unsafe { read_sections(base) }.is_none());
        assert!(unsafe { loaded_pe_debug_info(base, image.len() as u64) }.is_none());
        image[0] = 0;
        assert!(unsafe { read_sections(image.as_ptr() as u64) }.is_none());
    }

    #[test]
    fn macho_uuid_is_read_from_the_mapped_load_commands() {
        let mut header = vec![0u8; 32];
        header[0..4].copy_from_slice(&0xfeed_facfu32.to_le_bytes());
        header[16..20].copy_from_slice(&2u32.to_le_bytes());
        header[20..24].copy_from_slice(&(16u32 + 24).to_le_bytes());
        // An unrelated 16-byte command, then LC_UUID.
        header.extend_from_slice(&0x19u32.to_le_bytes());
        header.extend_from_slice(&16u32.to_le_bytes());
        header.extend_from_slice(&[0; 8]);
        header.extend_from_slice(&0x1bu32.to_le_bytes());
        header.extend_from_slice(&24u32.to_le_bytes());
        header.extend((1..=16).collect::<Vec<u8>>());
        let uuid = unsafe { loaded_macho_uuid(header.as_ptr()) }.expect("uuid");
        assert_eq!(uuid, core::array::from_fn(|index| index as u8 + 1));

        header[0] = 0;
        assert!(unsafe { loaded_macho_uuid(header.as_ptr()) }.is_none());
    }

    #[test]
    fn a_negative_slide_moves_addresses_down() {
        assert_eq!(add_slide(0x1000, 0x10), Some(0x1010));
        assert_eq!(add_slide(0x1000, -0x10), Some(0x0ff0));
        assert_eq!(add_slide(0x10, -0x20), None);
    }
}

#[cfg(test)]
#[path = "modules_host_tests.rs"]
mod host_tests;
