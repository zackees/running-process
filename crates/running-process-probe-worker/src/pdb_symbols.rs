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

/// Locate the PDB for `image`.
///
/// Only the sibling `<stem>.pdb` is considered. The path recorded inside the
/// PE points at wherever the binary was *built*, which on another machine is
/// either absent or — worse — a different build's file with the same name.
/// Honoring it would risk resolving addresses against symbols that do not
/// describe this binary, which is precisely the wrong-name failure this module
/// exists to avoid. Matching by recorded PDB GUID is #638's job.
fn pdb_path_for(image: &Path) -> Option<PathBuf> {
    let candidate = image.with_extension("pdb");
    candidate.is_file().then_some(candidate)
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
