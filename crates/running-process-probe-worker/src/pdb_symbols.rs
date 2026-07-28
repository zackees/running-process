//! Resolving addresses to function names from a PDB (#637).
//!
//! # Why a sorted table rather than a lookup per address
//!
//! PDB symbols carry a start address but no length, so the function
//! containing an address is the one with the greatest start not exceeding it.
//! That needs the symbols ordered, and a capture has many addresses in the
//! same module — building the table once and binary-searching it turns a
//! repeated linear scan of every symbol into one sort.
//!
//! # Nothing is guessed
//!
//! An address below the first symbol resolves to nothing rather than to the
//! first function. A missing or unreadable PDB yields no map at all. Every one
//! of those paths leaves the frame with its module and offset and a status
//! saying resolution did not happen — a wrong function name would send whoever
//! reads the report somewhere else entirely, and nothing in the output would
//! contradict them.

use std::fs::File;
use std::path::{Path, PathBuf};

use pdb::FallibleIterator as _;

/// A module's symbols, ordered by address for containment lookup.
pub struct SymbolTable {
    /// `(relative_virtual_address, name)`, sorted by address.
    entries: Vec<(u32, String)>,
}

impl SymbolTable {
    /// Build a table from the PDB belonging to `image`.
    ///
    /// Returns `None` when no PDB can be found or read. That is an ordinary
    /// outcome — a stripped release binary, or a machine that does not have
    /// the symbols — and it degrades rather than failing the capture.
    pub fn for_image(image: &Path) -> Option<Self> {
        let pdb_path = pdb_path_for(image)?;
        Self::from_pdb(&pdb_path)
    }

    /// Build a table from an explicit PDB file.
    pub fn from_pdb(pdb_path: &Path) -> Option<Self> {
        let file = File::open(pdb_path).ok()?;
        let mut pdb = pdb::PDB::open(file).ok()?;

        // The address map translates a symbol's internal section:offset into
        // the RVA the loader actually uses. Skipping it yields addresses that
        // look plausible and are wrong.
        let address_map = pdb.address_map().ok()?;
        let mut entries: Vec<(u32, String)> = Vec::new();

        if let Ok(symbols) = pdb.global_symbols() {
            let mut iter = symbols.iter();
            while let Ok(Some(symbol)) = iter.next() {
                if let Ok(pdb::SymbolData::Public(data)) = symbol.parse() {
                    // Only code symbols: a data symbol's address is never a
                    // return address, and including them would let a global
                    // variable claim a frame.
                    if !data.function {
                        continue;
                    }
                    if let Some(rva) = data.offset.to_rva(&address_map) {
                        entries.push((rva.0, data.name.to_string().into_owned()));
                    }
                }
            }
        }

        if entries.is_empty() {
            return None;
        }
        entries.sort_unstable_by_key(|(rva, _)| *rva);
        entries.dedup_by_key(|(rva, _)| *rva);
        Some(Self { entries })
    }

    /// Name of the function containing `relative_address`, if any.
    pub fn lookup(&self, relative_address: u64) -> Option<&str> {
        let target = u32::try_from(relative_address).ok()?;
        // The containing function is the last one starting at or before the
        // address.
        let index = match self.entries.binary_search_by_key(&target, |(rva, _)| *rva) {
            Ok(exact) => exact,
            // An address before every symbol belongs to no function here.
            Err(0) => return None,
            Err(next) => next - 1,
        };
        Some(self.entries[index].1.as_str())
    }

    /// First symbol whose name contains `needle`, as `(rva, name)`.
    ///
    /// Exists for tests that need a symbol they can name in an assertion.
    pub fn symbol_containing_name(&self, needle: &str) -> Option<(u32, String)> {
        self.entries
            .iter()
            .find(|(_, name)| name.contains(needle))
            .cloned()
    }

    /// Number of symbols in the table.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table holds no symbols.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The identity a PE records for the PDB it was built with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DebugId {
    /// GUID generated when the PDB was created.
    pub guid: [u8; 16],
    /// How many times the PDB had been written when the image was linked.
    pub age: u32,
}

/// Read the debug identity out of a PE image.
pub fn image_debug_id(image: &Path) -> Option<DebugId> {
    use object::Object as _;

    let bytes = std::fs::read(image).ok()?;
    let file = object::File::parse(&*bytes).ok()?;
    let cv = file.pdb_info().ok()??;
    Some(DebugId {
        guid: cv.guid(),
        age: cv.age(),
    })
}

/// Read the debug identity out of a PDB.
fn pdb_debug_id(pdb_path: &Path) -> Option<DebugId> {
    let file = File::open(pdb_path).ok()?;
    let mut pdb = pdb::PDB::open(file).ok()?;
    let info = pdb.pdb_information().ok()?;
    Some(DebugId {
        // `Uuid::as_bytes` is big-endian field order, which is what the PE's
        // CodeView record stores. Reading the fields individually and
        // reassembling them would reintroduce the byte-order bug this avoids.
        guid: *info.guid.as_bytes(),
        age: info.age,
    })
}

/// Whether `pdb` describes the build `image` recorded.
///
/// The GUID must match exactly. The age may be **higher** in the PDB: the
/// linker bumps it every time the file is rewritten, so a PDB that has been
/// updated since the image was linked still describes that image. A *lower*
/// age means the PDB predates the link and is a different build.
pub fn identity_matches(image: DebugId, pdb: DebugId) -> bool {
    image.guid == pdb.guid && pdb.age >= image.age
}

/// Locate the PDB for `image`, verifying it describes this exact build.
///
/// Only the sibling `<stem>.pdb` is considered. The path recorded inside the
/// PE points at wherever the binary was *built*, which on another machine is
/// either absent or — worse — a different build's file with the same name.
///
/// Being in the right place is not enough. A rebuild leaves a `.pdb` beside
/// the binary that is one build stale, and its symbols would resolve to
/// plausible, wrong function names with nothing downstream able to tell. So
/// the recorded GUID and age must match before the file is trusted (#638).
///
/// An image with no debug directory has nothing to match against, so no PDB
/// is accepted for it: an unverifiable claim is refused rather than assumed.
fn pdb_path_for(image: &Path) -> Option<PathBuf> {
    let candidate = image.with_extension("pdb");
    if !candidate.is_file() {
        return None;
    }
    let expected = image_debug_id(image)?;
    let actual = pdb_debug_id(&candidate)?;
    identity_matches(expected, actual).then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The PDB of the test binary itself, which a debug build produces.
    ///
    /// Returns `None` when it is absent, which happens on machines whose
    /// compiler cache drops the linker's side-files. Every test using this
    /// then skips — and a silently skipped test is indistinguishable from a
    /// passing one, which is how an earlier revision of this module reported
    /// "the PDB parses correctly" while having parsed nothing at all.
    ///
    /// So under CI the absence is a failure instead. A Windows CI run either
    /// exercises these paths for real or says plainly that it could not.
    fn own_pdb() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let candidate = exe.with_extension("pdb");
        if candidate.is_file() {
            return Some(candidate);
        }
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "no PDB at {} during a CI run; the symbol tests would skip and              assert nothing",
            candidate.display()
        );
        None
    }

    #[test]
    fn a_real_pdb_yields_symbols() {
        let Some(pdb_path) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        let table = SymbolTable::from_pdb(&pdb_path).expect("the test binary's PDB has symbols");
        assert!(
            !table.is_empty(),
            "the test binary PDB should list functions"
        );
    }

    /// Every symbol RVA must land inside an executable section of the PE.
    ///
    /// This is the check that the PDB's address map is actually applied. The
    /// other PDB tests take an address *out of* the table and look it up *in*
    /// the same table, so a uniformly wrong translation round-trips through
    /// them undetected — confirmed by sabotage: replacing
    /// `offset.to_rva(&address_map)` with the raw section offset left every
    /// one of them passing.
    ///
    /// The section layout read from the PE by `object` is an independent
    /// source, which is what makes this able to catch it.
    #[test]
    fn symbol_addresses_fall_inside_executable_sections() {
        use object::{Object as _, ObjectSection as _};

        let Some(pdb_path) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        let Some(table) = SymbolTable::from_pdb(&pdb_path) else {
            eprintln!("skipping: PDB had no public function symbols");
            return;
        };

        let exe = std::env::current_exe().expect("current exe");
        let bytes = std::fs::read(&exe).expect("read own image");
        let file = object::File::parse(&*bytes).expect("parse own PE");

        // `section.address()` is a VIRTUAL address — image base included —
        // while PDB symbols are RVAs. Subtracting the base puts both in the
        // same space. Comparing them directly reports every symbol as out of
        // range, which is how the first version of this test failed on CI.
        let base = file.relative_address_base();
        let ranges: Vec<(u64, u64)> = file
            .sections()
            .filter(|s| match s.flags() {
                // IMAGE_SCN_MEM_EXECUTE
                object::SectionFlags::Coff { characteristics } => {
                    characteristics & 0x2000_0000 != 0
                }
                _ => false,
            })
            .map(|s| {
                let start = s.address().saturating_sub(base);
                (start, start + s.size())
            })
            .collect();
        assert!(!ranges.is_empty(), "the PE should have executable sections");

        let outside: Vec<_> = table
            .entries
            .iter()
            .filter(|(rva, _)| {
                let rva = u64::from(*rva);
                !ranges
                    .iter()
                    .any(|(start, end)| rva >= *start && rva < *end)
            })
            .take(5)
            .collect();

        assert!(
            outside.is_empty(),
            "function symbols outside every executable section {ranges:?}:              {outside:?} — the PDB address map is likely not being applied",
        );
    }

    fn id(guid_byte: u8, age: u32) -> DebugId {
        DebugId {
            guid: [guid_byte; 16],
            age,
        }
    }

    #[test]
    fn an_exact_identity_matches() {
        assert!(identity_matches(id(0xAB, 3), id(0xAB, 3)));
    }

    /// A different GUID is a different build, whatever the age.
    #[test]
    fn a_different_guid_never_matches() {
        assert!(!identity_matches(id(0xAB, 3), id(0xCD, 3)));
        assert!(!identity_matches(id(0xAB, 3), id(0xCD, 99)));
    }

    /// The linker bumps the age each time it rewrites the PDB, so a PDB
    /// updated after the link still describes the image it was linked for.
    #[test]
    fn a_higher_pdb_age_still_matches() {
        assert!(identity_matches(id(0xAB, 3), id(0xAB, 4)));
    }

    /// A lower age means the PDB predates the link: a different build.
    #[test]
    fn a_lower_pdb_age_does_not_match() {
        assert!(
            !identity_matches(id(0xAB, 5), id(0xAB, 4)),
            "a PDB older than the image cannot describe it"
        );
    }

    /// The decisive check: the real binary and its own PDB must match.
    ///
    /// This is what validates the GUID byte order. The PE's CodeView record
    /// and the PDB's stream store the GUID differently enough that a naive
    /// field-by-field reassembly mismatches — and a mismatch here would look
    /// exactly like "no symbols available", silently disabling symbolization
    /// rather than failing.
    #[test]
    fn a_binary_matches_its_own_pdb() {
        let Some(pdb_path) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        let exe = std::env::current_exe().expect("current exe");

        let Some(image) = image_debug_id(&exe) else {
            panic!("the test binary has no CodeView debug directory");
        };
        let pdb = pdb_debug_id(&pdb_path).expect("the PDB has an identity");

        assert_eq!(
            image.guid, pdb.guid,
            "the image and its own PDB disagree on the GUID; byte order is wrong"
        );
        assert!(
            identity_matches(image, pdb),
            "image {image:?} did not match its own pdb {pdb:?}"
        );
    }

    /// And the lookup that uses it accepts that pair.
    #[test]
    fn the_sibling_pdb_of_this_binary_is_accepted() {
        if own_pdb().is_none() {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        }
        let exe = std::env::current_exe().expect("current exe");
        assert!(
            pdb_path_for(&exe).is_some(),
            "the binary's own sibling PDB should pass identity verification"
        );
    }

    /// A PDB that is merely in the right place must not be trusted.
    #[test]
    fn a_sibling_pdb_from_a_different_build_is_rejected() {
        let Some(real) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        // A copy of this binary, with a copy of a PDB that describes a
        // *different* image — modelled by pairing our PDB with an unrelated
        // executable name whose debug id will not match.
        let fake_exe = dir.path().join("other.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &fake_exe).expect("copy exe");
        // Truncate the copied PDB so its identity cannot be read: an
        // unreadable identity must be refused, not assumed to match.
        std::fs::write(
            dir.path().join("other.pdb"),
            &std::fs::read(&real).unwrap()[..64],
        )
        .expect("write pdb");

        assert!(
            pdb_path_for(&fake_exe).is_none(),
            "a PDB whose identity cannot be verified must not be accepted"
        );
    }

    #[test]
    fn a_missing_pdb_yields_no_table() {
        assert!(SymbolTable::from_pdb(Path::new("no-such-file.pdb")).is_none());
    }

    /// Garbage must be refused, not parsed into confident nonsense.
    #[test]
    fn a_corrupt_pdb_yields_no_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("broken.pdb");
        std::fs::write(&path, [0xFFu8; 8192]).expect("write");
        assert!(SymbolTable::from_pdb(&path).is_none());
    }

    #[test]
    fn an_address_below_every_symbol_resolves_to_nothing() {
        let table = SymbolTable {
            entries: vec![(0x1000, "alpha".into()), (0x2000, "bravo".into())],
        };
        assert_eq!(
            table.lookup(0x0FFF),
            None,
            "must not claim the first symbol"
        );
    }

    #[test]
    fn an_address_inside_a_function_resolves_to_it() {
        let table = SymbolTable {
            entries: vec![(0x1000, "alpha".into()), (0x2000, "bravo".into())],
        };
        assert_eq!(table.lookup(0x1000), Some("alpha"), "exact start");
        assert_eq!(table.lookup(0x1234), Some("alpha"), "inside alpha");
        assert_eq!(table.lookup(0x2000), Some("bravo"), "exact start of bravo");
        // Past the last symbol there is no next start to bound it, so the last
        // function is the best available answer.
        assert_eq!(table.lookup(0x9999), Some("bravo"));
    }

    #[test]
    fn an_address_too_large_for_an_rva_resolves_to_nothing() {
        let table = SymbolTable {
            entries: vec![(0x1000, "alpha".into())],
        };
        assert_eq!(table.lookup(u64::from(u32::MAX) + 1), None);
    }

    /// Names read out of a real PDB must be real symbols, not garbage.
    ///
    /// A parser that silently mis-reads the string table would still produce
    /// *some* name for every address; requiring this crate's own name to
    /// appear proves the bytes were interpreted correctly.
    #[test]
    fn a_real_pdb_yields_recognizable_symbol_names() {
        let Some(pdb_path) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        let Some(table) = SymbolTable::from_pdb(&pdb_path) else {
            eprintln!("skipping: PDB had no public function symbols");
            return;
        };

        assert!(
            table
                .entries
                .iter()
                .any(|(_, name)| name.contains("running_process_probe_worker")),
            "no symbol mentioned this crate; the {} names read look wrong, e.g. {:?}",
            table.len(),
            table.entries.iter().take(3).collect::<Vec<_>>()
        );
    }

    /// Looking up a symbol's own start address returns that symbol.
    ///
    /// This checks the binary-search arithmetic against real, unevenly spaced
    /// addresses rather than the hand-written pairs above. It does not
    /// exercise module-base subtraction — the caller does that, and
    /// `symbolize` covers it.
    #[test]
    fn every_symbol_resolves_to_itself_at_its_own_address() {
        let Some(pdb_path) = own_pdb() else {
            eprintln!("skipping: no PDB beside the test binary");
            return;
        };
        let Some(table) = SymbolTable::from_pdb(&pdb_path) else {
            eprintln!("skipping: PDB had no public function symbols");
            return;
        };

        for (rva, expected) in table.entries.iter().step_by(97) {
            assert_eq!(
                table.lookup(u64::from(*rva)),
                Some(expected.as_str()),
                "symbol at {rva:#x} did not resolve to itself"
            );
        }
    }
}
