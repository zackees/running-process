//! ELF/DWARF and Mach-O/dSYM discovery and function lookup (#638).
//!
//! The parser lives in the disposable worker for the same reason as the PDB
//! parser: malformed debug data must never enter the long-lived daemon.

use std::path::{Path, PathBuf};

use object::{Object as _, ObjectSection as _, ObjectSymbol as _};

use crate::discovery::{self, DiscoverySource, ResolveOutcome};
use crate::wire::{DiscoveryConfig, ModuleRef};

/// A verified ELF or Mach-O symbol source.
pub enum ModuleSymbols {
    /// Verified identity and a usable symbol table.
    Found {
        /// Parsed function symbols.
        table: SymbolTable,
        /// Verified local path or server URL.
        symbol_file: String,
        /// Winning discovery tier.
        source: DiscoverySource,
    },
    /// No candidate existed.
    NotFound,
    /// Candidates existed but failed exact identity or parsing.
    Mismatched {
        /// Number of rejected candidates.
        rejected: usize,
    },
    /// The module had no supported build identity.
    NoDebugDirectory,
}

/// Ordered function starts normalized to module-relative addresses.
pub struct SymbolTable {
    entries: Vec<(u64, String)>,
}

impl SymbolTable {
    #[cfg(test)]
    fn from_object(path: &Path) -> Option<Self> {
        let bytes = read_bounded(path)?;
        let file = object::File::parse(&*bytes).ok()?;
        Self::from_file(&file)
    }

    fn from_file(file: &object::File<'_>) -> Option<Self> {
        // Capture offsets are relative to the same image base used by the
        // module inventory: zero/load-bias for ELF and the Mach-O relative
        // address base for dyld images. Subtracting the first text section
        // would incorrectly shift every ELF symbol by its section offset.
        let image_base = file.relative_address_base();
        let mut entries = file
            .symbols()
            .chain(file.dynamic_symbols())
            .filter(|symbol| {
                symbol.kind() == object::SymbolKind::Text
                    && symbol.address() >= image_base
                    && !symbol.is_undefined()
            })
            .filter_map(|symbol| {
                Some((
                    symbol.address() - image_base,
                    symbol.name().ok()?.to_string(),
                ))
            })
            .collect::<Vec<_>>();
        entries.sort_unstable_by_key(|(address, _)| *address);
        entries.dedup_by_key(|(address, _)| *address);
        (!entries.is_empty()).then_some(Self { entries })
    }

    /// Resolve the containing function by module-relative address.
    pub fn lookup(&self, relative_address: u64) -> Option<&str> {
        let index = match self
            .entries
            .binary_search_by_key(&relative_address, |(address, _)| *address)
        {
            Ok(exact) => exact,
            Err(0) => return None,
            Err(next) => next - 1,
        };
        Some(self.entries[index].1.as_str())
    }
}

/// Discover and parse exact-build symbols for one ELF or Mach-O module.
pub fn discover_module(module: &ModuleRef, config: &DiscoveryConfig) -> ModuleSymbols {
    if module.path_hint.is_none() && module.debug_id.is_none() {
        return ModuleSymbols::NotFound;
    }
    if !crate::discovery::captured_image_still_matches(module) {
        return ModuleSymbols::Mismatched { rejected: 1 };
    }
    let Some(image_path) = module.path_hint.as_deref().map(Path::new) else {
        return ModuleSymbols::NoDebugDirectory;
    };
    let expected = module
        .debug_id
        .as_deref()
        .filter(|identity| identity.starts_with("elf:") || identity.starts_with("macho:"))
        .map(str::to_owned)
        .or_else(|| object_identity(image_path));
    let Some(expected) = expected else {
        return ModuleSymbols::NoDebugDirectory;
    };

    // Normal ELF/Mach-O debug sections live in the module itself. GNU
    // MiniDebugInfo is an XZ-compressed ELF in `.gnu_debugdata`; extract it
    // into worker-private storage before the normal identity/usability gate.
    let extracted = extracted_gnu_debugdata(image_path);
    let extracted_is_usable = extracted
        .as_ref()
        .is_some_and(|file| load_object_for_identity(file.path(), &expected).is_some());
    let embedded_path = if extracted_is_usable {
        extracted.as_ref().expect("checked above").path()
    } else {
        image_path
    };
    let mut with_embedded = module.clone();
    with_embedded.embedded_symbol_path = Some(embedded_path.to_string_lossy().into_owned());
    let native_name = native_symbol_name(image_path, &expected);
    let format = if expected.starts_with("macho:") {
        crate::discovery::SymbolArtifactFormat::MachoDsym
    } else {
        crate::discovery::SymbolArtifactFormat::ElfDwarf
    };
    let mut verified_table = None;
    let local = discovery::resolve_symbols(
        &with_embedded,
        config,
        &expected,
        format,
        &native_name,
        &configured_stores(),
        |candidate| {
            let Some(table) = load_object_for_identity(candidate, &expected) else {
                return false;
            };
            verified_table = Some(table);
            true
        },
    );
    match local {
        ResolveOutcome::Found(resolved) => {
            let Some(table) = verified_table.take() else {
                return ModuleSymbols::Mismatched { rejected: 1 };
            };
            ModuleSymbols::Found {
                table,
                symbol_file: resolved.path.to_string_lossy().into_owned(),
                source: resolved.source,
            }
        }
        ResolveOutcome::NotFound => server_symbols(&expected, &native_name, 0),
        ResolveOutcome::Mismatched {
            rejected: local_rejected,
        } => server_symbols(&expected, &native_name, local_rejected),
    }
}

fn server_symbols(expected: &str, native_name: &Path, local_rejected: usize) -> ModuleSymbols {
    match discovery::resolve_configured_server(expected, native_name, |path| {
        load_object_for_identity(path, expected)
    }) {
        discovery::ServerResolve::Found { url, value: table } => ModuleSymbols::Found {
            table,
            symbol_file: url,
            source: DiscoverySource::ConfiguredServer,
        },
        discovery::ServerResolve::NotFound if local_rejected == 0 => ModuleSymbols::NotFound,
        discovery::ServerResolve::NotFound => ModuleSymbols::Mismatched {
            rejected: local_rejected,
        },
        discovery::ServerResolve::Mismatched { rejected } => ModuleSymbols::Mismatched {
            rejected: local_rejected + rejected,
        },
    }
}

fn object_identity(path: &Path) -> Option<String> {
    let bytes = read_bounded(path)?;
    let file = object::File::parse(&*bytes).ok()?;
    identity_from_file(&file)
}

fn load_object_for_identity(path: &Path, expected: &str) -> Option<SymbolTable> {
    let bytes = read_bounded(path)?;
    let file = object::File::parse(&*bytes).ok()?;
    if identity_from_file(&file).as_deref() != Some(expected) {
        return None;
    }
    SymbolTable::from_file(&file)
}

fn identity_from_file(file: &object::File<'_>) -> Option<String> {
    let identity = if let Some(build_id) = file.build_id().ok()? {
        format!("elf:{}", hex(build_id))
    } else if let Some(uuid) = file.mach_uuid().ok()? {
        format!("macho:{}", hex(&uuid))
    } else {
        return None;
    };
    Some(identity)
}

fn extracted_gnu_debugdata(image: &Path) -> Option<tempfile::NamedTempFile> {
    let bytes = read_bounded(image)?;
    let file = object::File::parse(&*bytes).ok()?;
    let compressed = file.section_by_name(".gnu_debugdata")?.data().ok()?;
    decompress_xz_to_temp(compressed)
}

fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read as _;

    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(discovery::MAX_SYMBOL_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= discovery::MAX_SYMBOL_BYTES).then_some(bytes)
}

fn decompress_xz_to_temp(compressed: &[u8]) -> Option<tempfile::NamedTempFile> {
    use std::io::{Cursor, Error, ErrorKind, Write as _};

    struct BoundedWriter<W> {
        inner: W,
        written: u64,
    }

    impl<W: std::io::Write> std::io::Write for BoundedWriter<W> {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            let remaining = discovery::MAX_SYMBOL_BYTES.saturating_sub(self.written);
            if buffer.len() as u64 > remaining {
                return Err(Error::new(
                    ErrorKind::FileTooLarge,
                    "decompressed MiniDebugInfo exceeds symbol limit",
                ));
            }
            let written = self.inner.write(buffer)?;
            self.written += written as u64;
            Ok(written)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }

    let mut file = tempfile::NamedTempFile::new().ok()?;
    {
        let mut writer = BoundedWriter {
            inner: file.as_file_mut(),
            written: 0,
        };
        lzma_rs::xz_decompress(&mut Cursor::new(compressed), &mut writer).ok()?;
        writer.flush().ok()?;
    }
    Some(file)
}

fn native_symbol_name(image: &Path, identity: &str) -> PathBuf {
    let name = image
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("module"));
    if identity.starts_with("macho:") {
        let mut bundle = name.clone();
        bundle.as_mut_os_string().push(".dSYM");
        bundle
            .join("Contents")
            .join("Resources")
            .join("DWARF")
            .join(name)
    } else {
        let mut debug = name;
        debug.as_mut_os_string().push(".debug");
        debug
    }
}

fn configured_stores() -> Vec<PathBuf> {
    std::env::var_os("RUNNING_PROCESS_PROBE_SYMBOL_PATH").map_or_else(Vec::new, |value| {
        std::env::split_paths(&value)
            .filter(|path| !path.as_os_str().is_empty())
            .collect()
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn this_test_binary_has_a_typed_build_identity() {
        let exe = std::env::current_exe().unwrap();
        let identity = object_identity(&exe).expect("ELF/Mach-O test binary identity");
        assert!(
            identity.starts_with("elf:") || identity.starts_with("macho:"),
            "{identity}"
        );
    }

    #[test]
    fn embedded_symbols_from_this_exact_build_resolve() {
        let exe = std::env::current_exe().unwrap();
        let module = ModuleRef {
            name: exe.file_name().unwrap().to_string_lossy().into_owned(),
            path_hint: Some(exe.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let ModuleSymbols::Found { source, table, .. } =
            discover_module(&module, &DiscoveryConfig::default())
        else {
            panic!("the unstripped test binary must resolve its embedded symbols");
        };
        assert_eq!(source, DiscoverySource::Embedded);
        assert!(table
            .entries
            .iter()
            .any(|(_, name)| name.contains("embedded_symbols_from_this_exact_build_resolve")));
    }

    #[test]
    fn symbol_addresses_use_the_capture_module_base_convention() {
        let exe = std::env::current_exe().unwrap();
        let bytes = std::fs::read(&exe).unwrap();
        let file = object::File::parse(&*bytes).unwrap();
        let (expected_offset, expected_name) = file
            .symbols()
            .chain(file.dynamic_symbols())
            .filter(|symbol| symbol.kind() == object::SymbolKind::Text)
            .find_map(|symbol| {
                let name = symbol.name().ok()?;
                name.contains("symbol_addresses_use_the_capture_module_base_convention")
                    .then(|| {
                        (
                            symbol.address() - file.relative_address_base(),
                            name.to_owned(),
                        )
                    })
            })
            .expect("this test function must be present in the object symbol table");
        let table = SymbolTable::from_object(&exe).unwrap();
        assert_eq!(table.lookup(expected_offset), Some(expected_name.as_str()));
    }

    #[test]
    fn gnu_minidebug_xz_extraction_round_trips_in_worker_private_storage() {
        let payload = b"bounded mini debug payload";
        let mut compressed = Vec::new();
        lzma_rs::xz_compress(&mut Cursor::new(payload), &mut compressed).unwrap();
        let extracted = decompress_xz_to_temp(&compressed).expect("extract xz payload");
        assert_eq!(std::fs::read(extracted.path()).unwrap(), payload);
    }
}
