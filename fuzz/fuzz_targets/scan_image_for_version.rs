//! Fuzz the on-disk version scan over arbitrary bytes.
//!
//! `scan_image_for_version` is the one gcscope entry point fed bytes it does not control,
//! and below 3.11 the sole version source. It is more than the string grammar: goblin
//! locates the read-only data section first, so this covers PE/ELF/Mach-O header parsing,
//! `parse_macho`'s fat-slice arithmetic and the section-range clamping.
//!
//! The property is that it returns rather than panics — a crash here turns a diagnosable
//! "could not detect Python version" into a stack trace on every subcommand. Which version
//! comes back is pinned by the unit sweep and the live image test (ADR 0005).
//!
//! Run: `cargo +nightly fuzz run scan_image_for_version`.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let found = gcscope::remote_debugging::version::scan_image_for_version(data);

    // Cheap, and it makes the fuzzer hunt for a parse path that bypasses the grammar
    // rather than only for panics. A serial past the 4-bit field would name a build that
    // resolves to another build's layout (ADR 0012).
    if let Some(v) = found {
        assert!(v.serial <= 0xF, "serial {} does not fit the hex", v.serial);
        assert!(
            matches!(v.release_level, 0xA | 0xB | 0xC | 0xF),
            "release level {:#x} is not one the grammar can produce",
            v.release_level
        );
    }
});
