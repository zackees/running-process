//! Host-specific checks of the loaded-module inventory (#635, #974).
//!
//! Each module asserts what the real loader of one host reports for this very
//! test binary -- a PE CodeView identity, an ELF PT_NOTE build id, a Mach-O
//! LC_UUID -- so the host selection here is the behaviour under test. The
//! format parsing itself is tested host-neutrally in `modules.rs`.

#[cfg(windows)]
mod tests {
    use super::super::*;

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
    fn loaded_pe_owner_carries_its_codeview_identity() {
        let addr = (landmark as fn() -> u64) as usize as u64;
        let modules = enumerate_modules().expect("enumerate");
        let owner = module_for_address(&modules, addr).expect("owning module");
        assert!(
            owner
                .debug_id
                .as_deref()
                .is_some_and(|identity| identity.starts_with("pdb:")),
            "loaded PE did not expose its mapped CodeView GUID+age: {:?}",
            owner.debug_id
        );
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

#[cfg(target_os = "linux")]
mod linux_tests {
    use super::super::*;

    #[inline(never)]
    fn landmark() {}

    #[test]
    fn loaded_elf_owner_carries_its_pt_note_build_id() {
        let address = landmark as fn() as usize as u64;
        let modules = enumerate_modules().expect("enumerate");
        let owner = module_for_address(&modules, address).expect("owning module");
        assert!(
            owner
                .debug_id
                .as_deref()
                .is_some_and(|identity| identity.starts_with("elf:")),
            "loaded ELF did not expose its PT_NOTE build-id: {:?}",
            owner.debug_id
        );
    }
}

#[cfg(target_os = "macos")]
mod macos_tests {
    use super::super::*;

    #[inline(never)]
    fn landmark() {}

    #[test]
    fn loaded_macho_owner_carries_its_lc_uuid() {
        let address = landmark as fn() as usize as u64;
        let modules = enumerate_modules().expect("enumerate");
        let owner = module_for_address(&modules, address).expect("owning module");
        assert!(
            owner
                .debug_id
                .as_deref()
                .is_some_and(|identity| identity.starts_with("macho:")),
            "loaded Mach-O did not expose its LC_UUID: {:?}",
            owner.debug_id
        );
    }
}
