//! Turn raw captures into return addresses (#635).
//!
//! # This runs after every thread is resumed
//!
//! Unwinding never touches a live thread. It reads only the register values
//! and stack bytes copied during the suspend window, so it can allocate,
//! take locks, and take as long as it needs — the target is already running.
//! That separation is the reason the capture path can stay as small as it is.
//!
//! The mechanism is `read_stack`: framehop asks for a `u64` at an address, and
//! we answer out of the copied slice at `addr - stack_pointer`. Addresses
//! outside the copied window return `Err`, which framehop treats as the end of
//! what it can walk. A truncated capture therefore yields a shorter stack
//! rather than a wrong one.
//!
//! # Addresses, not symbols
//!
//! The output is return addresses. Resolving them to function names is
//! symbolization, which happens off-process in a later slice.

use std::ops::Range;

use framehop::x86_64::{CacheX86_64, UnwindRegsX86_64, UnwinderX86_64};
use framehop::{Module, ModuleSectionInfo, Unwinder};

use super::modules::LoadedModule;
use super::{Snapshot, ThreadSample};

/// Sections framehop may ask for. `.pdata`/`.xdata` carry the x86_64 unwind
/// tables; `.text` anchors code addresses.
const WANTED_SECTIONS: &[&str] = &[".text", ".pdata", ".xdata"];

/// Adapts a [`LoadedModule`] to framehop's section interface.
///
/// Section bytes are read straight from the mapped image — the module is in
/// our own address space, so this is a slice, not file I/O.
struct MappedModuleSections {
    base: u64,
    sections: Vec<(String, Range<u64>)>,
}

impl MappedModuleSections {
    fn new(module: &LoadedModule) -> Self {
        Self {
            base: module.base,
            sections: module
                .sections
                .iter()
                .filter(|s| WANTED_SECTIONS.contains(&s.name.as_str()))
                .map(|s| (s.name.clone(), s.range.clone()))
                .collect(),
        }
    }

    fn find(&self, name: &[u8]) -> Option<Range<u64>> {
        let name = std::str::from_utf8(name).ok()?;
        self.sections
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, r)| r.clone())
    }
}

impl ModuleSectionInfo<Vec<u8>> for MappedModuleSections {
    fn base_svma(&self) -> u64 {
        // For PE this is the image base, and because we read the *mapped*
        // image its stated and actual bases coincide.
        self.base
    }

    fn section_svma_range(&mut self, name: &[u8]) -> Option<Range<u64>> {
        self.find(name)
    }

    fn section_data(&mut self, name: &[u8]) -> Option<Vec<u8>> {
        let range = self.find(name)?;
        let len = usize::try_from(range.end.saturating_sub(range.start)).ok()?;
        if len == 0 {
            return None;
        }
        // Safety: the range came from the mapped module's own section table,
        // so it is committed memory inside this process for as long as the
        // module stays loaded.
        #[allow(unsafe_code)]
        let bytes = unsafe { std::slice::from_raw_parts(range.start as *const u8, len) }.to_vec();
        Some(bytes)
    }
}

/// Build an unwinder covering `modules`.
pub fn build_unwinder(modules: &[LoadedModule]) -> UnwinderX86_64<Vec<u8>> {
    let mut unwinder = UnwinderX86_64::new();
    for module in modules {
        let range = module.range();
        unwinder.add_module(Module::new(
            format!("{:#x}", module.base),
            range.clone(),
            module.base,
            MappedModuleSections::new(module),
        ));
    }
    unwinder
}

/// Unwind one captured thread into return addresses.
///
/// Reads only `sample`'s copied bytes; the thread itself is long since
/// resumed.
pub fn unwind_sample(
    unwinder: &UnwinderX86_64<Vec<u8>>,
    cache: &mut CacheX86_64,
    sample: &ThreadSample,
) -> Vec<u64> {
    let sp = sample.stack_pointer;
    let bytes = &sample.stack_bytes;

    // Answer stack reads out of the copy. Anything outside the captured window
    // is Err, which ends the walk — a truncated capture yields a shorter
    // stack, never a fabricated one.
    let mut read_stack = |addr: u64| -> Result<u64, ()> {
        let offset = addr.checked_sub(sp).ok_or(())?;
        let offset = usize::try_from(offset).map_err(|_| ())?;
        let end = offset.checked_add(8).ok_or(())?;
        if end > bytes.len() {
            return Err(());
        }
        let mut word = [0u8; 8];
        word.copy_from_slice(&bytes[offset..end]);
        Ok(u64::from_le_bytes(word))
    };

    let regs = UnwindRegsX86_64::new(
        sample.instruction_pointer,
        sample.stack_pointer,
        sample.frame_pointer,
    );

    let mut frames = Vec::new();
    let mut iter = unwinder.iter_frames(sample.instruction_pointer, regs, cache, &mut read_stack);
    while let Ok(Some(frame)) = iter.next() {
        frames.push(frame.address());
    }
    frames
}

/// Unwind every sample in `snapshot`, recording the result.
///
/// Sets `frames_resolved` only here — the one place frames actually exist.
pub fn resolve_frames(snapshot: &mut Snapshot, modules: &[LoadedModule]) {
    let unwinder = build_unwinder(modules);
    let mut cache = CacheX86_64::new();

    for sample in &mut snapshot.threads {
        sample.frames = unwind_sample(&unwinder, &mut cache, sample);
    }
    snapshot.frames_resolved = true;
}

#[cfg(test)]
mod tests {
    use super::super::modules::{enumerate_modules, module_for_address};
    use super::super::{capture_all_threads, SnapshotConfig};
    use super::*;

    #[inline(never)]
    fn inner_frame(flag: &std::sync::atomic::AtomicBool) {
        // Spin briefly so the sibling capture observes this frame on the stack.
        while !flag.load(std::sync::atomic::Ordering::Relaxed) {
            std::hint::spin_loop();
        }
    }

    #[test]
    fn unwinder_covers_every_enumerated_module() {
        let modules = enumerate_modules().expect("modules");
        let unwinder = build_unwinder(&modules);
        // Construction must not panic and must accept every module we found.
        // (framehop has no public module count, so this asserts the build path
        // is total rather than a specific number.)
        let _ = unwinder;
        assert!(!modules.is_empty());
    }

    #[test]
    fn reads_outside_the_captured_window_end_the_walk() {
        let sample = ThreadSample {
            os_tid: 1,
            stack_pointer: 0x1000,
            instruction_pointer: 0x2000,
            frame_pointer: 0,
            // Deliberately tiny: any read past 16 bytes must fail rather than
            // read adjacent memory.
            stack_bytes: vec![0u8; 16],
            truncated: true,
            kind: super::super::CaptureKind::RawContext,
            frames: Vec::new(),
        };
        let modules = enumerate_modules().expect("modules");
        let unwinder = build_unwinder(&modules);
        let mut cache = CacheX86_64::new();

        // Must terminate, not panic or spin, on a stack it cannot follow.
        let frames = unwind_sample(&unwinder, &mut cache, &sample);
        assert!(
            frames.len() <= 2,
            "a 16-byte stack cannot yield a deep walk, got {}",
            frames.len()
        );
    }

    /// The real check: unwind a live capture and confirm at least one returned
    /// address lands inside a module's `.text`.
    ///
    /// The oracle is the independently-verified module inventory from #700 —
    /// the unwinder is not allowed to confirm itself.
    #[test]
    fn unwound_addresses_fall_inside_known_text_sections() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || inner_frame(&stop))
        };
        std::thread::sleep(std::time::Duration::from_millis(50));

        let mut snapshot = capture_all_threads(&SnapshotConfig::default()).expect("capture");
        let modules = enumerate_modules().expect("modules");
        resolve_frames(&mut snapshot, &modules);

        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();

        assert!(snapshot.frames_resolved, "resolve_frames must mark this");

        let mut in_text = 0usize;
        for sample in &snapshot.threads {
            for &addr in &sample.frames {
                if let Some(m) = module_for_address(&modules, addr) {
                    if m.section(".text").is_some_and(|t| t.range.contains(&addr)) {
                        in_text += 1;
                    }
                }
            }
        }

        assert!(
            in_text > 0,
            "no unwound address landed in any module's .text; frames were {:?}",
            snapshot
                .threads
                .iter()
                .map(|s| s.frames.len())
                .collect::<Vec<_>>()
        );
    }
}
