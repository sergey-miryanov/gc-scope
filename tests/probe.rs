//! Read a `gcscope_probe` Ring out of a live interpreter and assert the decoded shape. This
//! is the Probe's correctness gate, replacing the prototype's `verify/` binary, which existed
//! only so a separate repository could reach gcscope's decoder across a path dependency
//! (`docs/adr/0016-probe-ships-from-this-repo.md`).
//!
//! **The seam is the region bytes, read from outside the process.** Asserting through the
//! Probe's Python-level `geometry()` would test the one surface no consumer uses. Downstream
//! of the address this runs gcscope's own memory reader, `OffsetTable` and ring decoder, so a
//! decode that passes here passes in the CLI.
//!
//! Only the address is supplied by hand: below 3.15 CPython publishes no
//! `gc.generation_stats` for gcscope's discovery path, and nothing in the interpreter points
//! at a Probe region. `specs/0014-read-probe-regions.md` moves that lookup into gcscope.
//!
//! `#[ignore]`d like the other live-attach tests (ADR 0005 §3):
//!   cargo test --test probe -- --ignored
//! With no Probe installed it skips; see `common::probe_python`.

mod common;

use common::{SpawnedPython, full_python_version, is_free_threaded, probe_python, probe_required};
use gcscope::memory::binary::elf_load_bias;
use gcscope::memory::reader::{open_handle, read_memory_h};
use gcscope::memory::regions::list_regions;
use gcscope::remote_debugging::gc_stats::GcStat;
use gcscope::remote_debugging::offsets::offset_table::{
    GcItemLayout, GcStatsKind, OffsetTable, compute_ring_base_offsets,
};
use read_process_memory::ProcessHandle;
use std::thread;
use std::time::Duration;

/// The Probe module's filename prefix, a wire contract rather than a detail: gcscope's
/// discovery matches on it (ADR 0014), and the module name in `gcscope_probe/src` fixes it.
const MODULE_PREFIX: &str = "gcscope_probe";

/// What CPython names a built extension module. macOS uses `.so` for extensions as well,
/// whatever it uses elsewhere.
#[cfg(windows)]
const MODULE_SUFFIX: &str = ".pyd";
#[cfg(not(windows))]
const MODULE_SUFFIX: &str = ".so";

/// The exported discovery anchor.
const HEADER_SYMBOL: &str = "gcscope_probe_header";

/// Samples across the run, and the gap between them. Three samples check a counter sequence
/// rather than a pair; the fixture collects gen 0 every ~50 ms, so this gap guarantees
/// progress without slowing the test.
const SAMPLES: usize = 3;
const SAMPLE_GAP: Duration = Duration::from_millis(700);

/// Real counters stay far below this; garbage from a wrong address rarely does. Same bound as
/// the live-smoke shape check.
const SANE_COUNTER_MAX: i64 = 1_000_000_000_000; // 1e12

/// Floor for a written entry's `heap_size`, the count of GC-tracked objects the Probe snapshots
/// when a Collection starts.
///
/// What holds it up is the interpreter's own baseline, not the fixture's garbage. Only
/// generation 0 snapshots the fresh burst: `probe_spin.py` collects generations 1 and 2
/// immediately after `gc.collect(0)` has already reclaimed it, so those entries see roughly
/// what a started interpreter tracks. Measured on 3.14, that is ~6800 for generations 1 and 2
/// against ~8500 for generation 0. The floor sits below both by a factor of five, and is not
/// a number to raise on the assumption that 6000 fresh objects are always in view.
///
/// The margin is what makes the check worth having: everything following `heap_size` in
/// `_gc_runtime_state` is a near-zero counter — `work_to_do` and `phase` on 3.14.4, the `dummy`
/// placeholders that replaced them on 3.14.5 — so an offset one field out reads a small number
/// that a bare positivity check waves through.
const MIN_TRACKED_OBJECTS: i64 = 1_000;

/// The 3.15 `gc_generation_stats` field layout the Probe writes.
///
/// Restated rather than imported: gcscope's generated `v_3_15_0b4::GC_LAYOUT` is private, and
/// this has to match the *Probe's* struct, a separate thing that agrees with it today.
/// `specs/0012-gen-offsets-serves-the-probe.md` gives the Probe a generated header to assert
/// against, and ticket 09 gives this side the digest to check the same fact.
static FIELDS: &[(&str, usize)] = &[
    ("ts_start", 0),
    ("ts_stop", 8),
    ("collections", 16),
    ("collected", 24),
    ("uncollectable", 32),
    ("candidates", 40),
    ("duration", 48),
    ("heap_size", 56),
];

static LAYOUT: GcItemLayout = GcItemLayout {
    item_size: 64,
    fields: FIELDS,
};

/// The `gcscope_probe_header` a live target publishes, decoded. Walking the slots and decoding
/// a region needs nothing beyond this, so no geometry passes out of band.
#[derive(Debug)]
struct ProbeHeader {
    version: u32,
    slots_addr: u64,
    max_interp: u32,
    slot_stride: u32,
    slot_claimed_off: u32,
    slot_interp_id_off: u32,
    stats_off_in_slot: u32,
    item_size: u32,
    region_size: u32,
    young_entries: u32,
    old_entries: u32,
    py_version: u32,
    collector: u32,
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().expect("4 bytes in range"))
}
fn u64_at(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().expect("8 bytes in range"))
}

/// Locate `gcscope_probe_header` in a live process from what the OS and the on-disk image say:
/// find the mapped Probe module, parse its symbol table, rebase the symbol onto the module's
/// load address. No scanning and no hardcoded address, which is the chain ADR 0014 specifies.
///
/// One goblin match covers PE and ELF, so both platforms read the same fact through one lookup.
fn find_header_addr(pid: u32) -> Result<(String, u64), String> {
    let regions = list_regions(pid).map_err(|e| format!("listing target regions: {e}"))?;

    let mut module: Option<(String, u64)> = None;
    for r in &regions {
        let Some(path) = r.filename().and_then(|p| p.to_str()) else {
            continue;
        };
        let name = path
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(path)
            .to_lowercase();
        if !(name.starts_with(MODULE_PREFIX) && name.ends_with(MODULE_SUFFIX)) {
            continue;
        }
        // An image's lowest mapping is where its first loadable segment landed, on both
        // formats here. The load bias below accounts for what that address is relative to.
        let entry = (path.to_string(), r.start() as u64);
        if module.as_ref().is_none_or(|(_, base)| entry.1 < *base) {
            module = Some(entry);
        }
    }
    let (path, base) = module.ok_or_else(|| {
        format!("no {MODULE_PREFIX}*{MODULE_SUFFIX} is mapped in the target process")
    })?;

    let bytes = std::fs::read(&path).map_err(|e| format!("reading {path}: {e}"))?;
    let addr = match goblin::Object::parse(&bytes) {
        Ok(goblin::Object::PE(pe)) => pe
            .exports
            .iter()
            .find(|e| e.name == Some(HEADER_SYMBOL))
            .map(|e| base + e.rva as u64),
        Ok(goblin::Object::Elf(elf)) => {
            // `dynsyms` only. The static `.symtab` carries the symbol whatever its visibility,
            // so a fallback there would let a `-fvisibility=hidden` regression pass here while
            // breaking discovery in the field. gcscope's `resolve_symbol_elf` does fall back,
            // for a different question. This lookup asserts the symbol sits where a remote
            // reader will look.
            let bias = elf_load_bias(&elf)
                .ok_or_else(|| format!("{path} has no PT_LOAD segment to take a load bias from"))?;
            elf.dynsyms
                .iter()
                .find(|s| elf.dynstrtab.get_at(s.st_name) == Some(HEADER_SYMBOL))
                .map(|s| base.wrapping_add(s.st_value.wrapping_sub(bias)))
        }
        // Mach-O belongs to spec 0013's port. Nothing here has run on macOS, and an untested
        // arm reads as support, so ticket 04 adds it against a leg that can prove it.
        Ok(other) => {
            return Err(format!(
                "{path} parses as {}, which this lookup does not handle yet",
                object_kind(&other)
            ));
        }
        Err(e) => return Err(format!("parsing {path}: {e}")),
    };

    // ADR 0014 §2 names this failure: a module that works and is invisible to discovery. Say
    // which of the two happened.
    let addr = addr.ok_or_else(|| {
        format!(
            "{path} does not export {HEADER_SYMBOL}; the module built but its discovery anchor \
             is not in the table a remote reader consults"
        )
    })?;

    Ok((path, addr))
}

/// Names the format goblin found, for the error above.
fn object_kind(obj: &goblin::Object) -> &'static str {
    match obj {
        goblin::Object::Mach(_) => "Mach-O",
        goblin::Object::Archive(_) => "an archive",
        goblin::Object::COFF(_) => "COFF",
        _ => "a format goblin does not recognise",
    }
}

/// Decode the header, refusing rather than clamping on anything implausible (ADR 0012).
fn read_header(handle: &ProcessHandle, addr: u64) -> Result<ProbeHeader, String> {
    // Fixed prefix (magic + header_size) first, then the whole struct at its declared size.
    let head =
        read_memory_h(handle, addr, 16).map_err(|e| format!("reading header prefix: {e}"))?;
    if &head[0..8] != b"GCPRB015" {
        return Err(format!(
            "bad magic at {addr:#x}: {:?} (expected GCPRB015)",
            String::from_utf8_lossy(&head[0..8])
        ));
    }
    let header_size = u32_at(&head, 8) as usize;
    if !(68..=512).contains(&header_size) {
        return Err(format!("implausible header_size {header_size}"));
    }
    let b = read_memory_h(handle, addr, header_size).map_err(|e| format!("reading header: {e}"))?;

    Ok(ProbeHeader {
        version: u32_at(&b, 12),
        slots_addr: u64_at(&b, 16),
        max_interp: u32_at(&b, 24),
        slot_stride: u32_at(&b, 28),
        slot_claimed_off: u32_at(&b, 32),
        slot_interp_id_off: u32_at(&b, 36),
        stats_off_in_slot: u32_at(&b, 40),
        item_size: u32_at(&b, 44),
        region_size: u32_at(&b, 48),
        young_entries: u32_at(&b, 52),
        old_entries: u32_at(&b, 56),
        py_version: u32_at(&b, 60),
        collector: u32_at(&b, 64),
    })
}

/// Every claimed slot, as `(interpreter id, region address)`.
fn live_regions(handle: &ProcessHandle, h: &ProbeHeader) -> Result<Vec<(i64, u64)>, String> {
    let mut out = Vec::new();
    for i in 0..h.max_interp as u64 {
        let slot = h.slots_addr + i * h.slot_stride as u64;
        let claimed = read_memory_h(handle, slot + h.slot_claimed_off as u64, 4)
            .map_err(|e| format!("reading slot {i} claimed flag: {e}"))?;
        if u32_at(&claimed, 0) == 0 {
            continue;
        }
        let idb = read_memory_h(handle, slot + h.slot_interp_id_off as u64, 8)
            .map_err(|e| format!("reading slot {i} interpreter id: {e}"))?;
        out.push((u64_at(&idb, 0) as i64, slot + h.stats_off_in_slot as u64));
    }
    Ok(out)
}

/// An `OffsetTable` describing the region the target declared. Every geometry input comes from
/// the header; only the per-entry field layout stays local, since the header carries none yet.
fn ring_table(h: &ProbeHeader) -> OffsetTable {
    let item_size = h.item_size as u64;
    let entries = [
        h.young_entries as u64,
        h.old_entries as u64,
        h.old_entries as u64,
    ];
    OffsetTable {
        version_hex: h.py_version as u64,
        runtime_interpreters_head: 0,
        runtime_gc: None,
        interp_next: 0,
        interp_id: 0,
        interp_threads_head: 0,
        interp_gc: None,
        thread_interp: 0,
        gc_generations: 0,
        gc_collecting: 0,
        gc_frame: None,
        gc_stats_kind: GcStatsKind::RingBuffer,
        gc_layout: Some(&LAYOUT),
        gc_stats_addr: None,
        gc_item_size: Some(item_size),
        gc_entries_per_gen: Some(entries),
        gc_gen_base_offsets: Some(compute_ring_base_offsets(item_size, &entries)),
        gc_stats_inline_off: 0,
        gc_stats_addr_is_per_interp: true,
    }
}

/// Newest entry per generation: the largest `collections`, since Records are cumulative and
/// the Ring wraps. Mirrors how a real consumer picks the head.
fn newest_per_gen<'a>(written: &[&'a GcStat]) -> Vec<&'a GcStat> {
    (0..3u32)
        .filter_map(|g| {
            written
                .iter()
                .copied()
                .filter(|s| s.generation == g)
                .max_by_key(|s| s.collections())
        })
        .collect()
}

/// Assert the decoded table's **shape** rather than a successful read. A Probe writing at a
/// wrong offset publishes a full table of plausible garbage that any non-empty check waves
/// through, so this checks the exact `(generation, entry)` index set the declared geometry
/// implies, plausible magnitudes, and a relationship the fixture controls.
fn check_shape(stats: &[GcStat], entries: [usize; 3]) -> Result<(), String> {
    let want: usize = entries.iter().sum();
    if stats.len() != want {
        return Err(format!(
            "declared geometry implies {want} entries {entries:?}, decoded {}",
            stats.len()
        ));
    }

    // Every (generation, entry) pair appears once, which catches a base offset that aliases
    // two generations onto the same entry range.
    let mut got: Vec<(u32, usize)> = stats.iter().map(|s| (s.generation, s.index)).collect();
    got.sort_unstable();
    let mut expect: Vec<(u32, usize)> = Vec::with_capacity(want);
    for (g, &n) in entries.iter().enumerate() {
        for e in 0..n {
            expect.push((g as u32, e));
        }
    }
    expect.sort_unstable();
    if got != expect {
        return Err(format!("wrong (generation, entry) set: {got:?}"));
    }

    for s in stats {
        for (name, v) in [
            ("collections", s.collections()),
            ("collected", s.collected()),
            ("uncollectable", s.uncollectable()),
            ("candidates", s.candidates()),
            ("heap_size", s.heap_size()),
        ] {
            if !(0..=SANE_COUNTER_MAX).contains(&v) {
                return Err(format!(
                    "gen {} entry {}: implausible {name}={v} (reading the wrong address?)",
                    s.generation, s.index
                ));
            }
        }
    }

    // The pyramid. probe_spin.py seeds 20/5/1 into generations 0/1/2 and holds that weighting,
    // making this deterministic. It catches a right-shaped table carrying another generation's
    // data, which the index-set check above cannot.
    let peak: Vec<i64> = (0..3u32)
        .map(|g| {
            stats
                .iter()
                .filter(|s| s.generation == g)
                .map(|s| s.collections())
                .max()
                .unwrap_or(0)
        })
        .collect();
    if !(peak[0] > peak[1] && peak[1] > peak[2]) {
        return Err(format!(
            "generation collections {peak:?} are not a strict pyramid; generations may be \
             aliased, or the Probe missed one"
        ));
    }
    Ok(())
}

/// The four invariants the prototype's verifier established, asserted against Records read
/// out of the process: every written entry decodes, passes `is_complete()`, carries a
/// positive duration, and cumulative counters never regress between samples. A fifth checks
/// `heap_size`, the one field the Probe reaches by raw offset into an internal struct.
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport and an installed Probe — run with --ignored"]
fn probe_ring_decodes_out_of_process() {
    let Some(python) = probe_python() else {
        // "installed", not "importable": the source directory in the crate root imports as a
        // namespace package on any interpreter, so the check calls into the module.
        let msg = "no interpreter with the `gcscope_probe` extension installed \
                   (build it: pip install ./gcscope_probe)";
        assert!(
            !probe_required(),
            "GCSCOPE_REQUIRE_PROBE=1 but {msg}; a leg that builds a Probe must not pass by \
             skipping"
        );
        eprintln!("SKIP probe_ring_decodes_out_of_process: {msg}");
        return;
    };
    // A free-threaded build never maintains `heap_size`, so the Probe refuses to load there
    // rather than publish a column of zeros. Reaching this assertion means it did load, which
    // is a broken gate rather than an unsupported configuration — so it fails loudly instead
    // of skipping.
    assert!(
        !is_free_threaded(&python),
        "GCSCOPE_TEST_PYTHON selected a free-threaded build and the Probe imported anyway; \
         the Py_GIL_DISABLED gate in PyInit_gcscope_probe is not doing its job"
    );

    let proc = SpawnedPython::spawn_fixture(&python, "probe_spin.py")
        .expect("probe_spin.py should reach READY");
    let pid = proc.pid();

    let (module, header_addr) = find_header_addr(pid).expect("locate the published header");
    let handle = open_handle(pid).expect("open the target process");
    let h = read_header(&handle, header_addr).expect("decode the published header");

    // Ask the target rather than hardcode the answer: the header's py_version must match what
    // the interpreter reports about itself. A Probe loaded into the wrong runtime is what the
    // load gate exists to prevent, and this is where that shows.
    if let Some((major, minor, micro, level, serial)) = full_python_version(&python) {
        let expected = ((major as u32) << 24)
            | ((minor as u32) << 16)
            | ((micro as u32) << 8)
            | ((level as u32) << 4)
            | serial as u32;
        assert_eq!(
            h.py_version, expected,
            "header py_version {:#010x} disagrees with the interpreter's own {expected:#010x} \
             ({module})",
            h.py_version
        );
    }
    assert_eq!(h.version, 3, "unexpected region header version");
    assert_eq!(
        h.item_size as usize, LAYOUT.item_size,
        "target item size {} disagrees with this reader's layout {}",
        h.item_size, LAYOUT.item_size
    );
    assert!(
        h.collector <= 1,
        "header declares collector {}, a value this build does not define",
        h.collector
    );

    let regions = live_regions(&handle, &h).expect("walk the slot array");
    let (iid, addr) = *regions
        .first()
        .expect("no claimed interpreter slot; the callback never ran");

    let table = ring_table(&h);
    let expected_len = table
        .stats_buffer_len()
        .expect("ring geometry is decodable");
    // gcscope's `stats_buffer_len` stops at the end of gen 2's entries, while the producer's
    // region also carries gen 2's trailing cursor word, making it 8 bytes longer. A superset
    // is correct; a shorter region would mean the two disagree about geometry.
    assert!(
        h.region_size as usize >= expected_len,
        "declared region {} is smaller than its own geometry implies ({expected_len})",
        h.region_size
    );

    let entries = [
        h.young_entries as usize,
        h.old_entries as usize,
        h.old_entries as usize,
    ];
    let mut prev_totals: Option<Vec<i64>> = None;
    let mut first_gen0: Option<i64> = None;
    let mut last_gen0 = 0i64;

    for round in 1..=SAMPLES {
        thread::sleep(SAMPLE_GAP);

        // gcscope's reader, gcscope's decoder. The only local step was the address.
        let raw = read_memory_h(&handle, addr, expected_len)
            .unwrap_or_else(|e| panic!("sample {round}: reading the region: {e}"));
        let stats = table.decode_gc_stats(&raw, iid);

        if let Err(e) = check_shape(&stats, entries) {
            panic!("sample {round}: {e}");
        }

        // Invariants 1 and 2: every *written* entry is a finished Collection, ts_start <
        // ts_stop. Unwritten entries are zeroed, so `collections() > 0` excludes them instead
        // of weakening the predicate to accommodate them.
        let written: Vec<&GcStat> = stats.iter().filter(|s| s.collections() > 0).collect();
        assert!(
            !written.is_empty(),
            "sample {round}: no entry carries a collection; the Ring is shaped but empty"
        );
        for s in &written {
            assert!(
                s.is_complete(),
                "sample {round}: gen {} entry {} written but not complete: \
                 ts_start={} ts_stop={}",
                s.generation,
                s.index,
                s.ts_start(),
                s.ts_stop()
            );
            // Invariant 3. `duration` is cumulative, so a written entry carries some.
            assert!(
                s.duration() > 0.0,
                "sample {round}: gen {} entry {} has non-positive duration {}",
                s.generation,
                s.index,
                s.duration()
            );
            // Invariant 5: `heap_size` came from the interpreter rather than from an offset
            // pointing at something else. This is the field the Probe reaches by byte offset
            // into a struct with no accessor, so it is the one the compiled-in offsets of
            // ADR 0013 exist for. Reading 0 here is what a wheel carrying another platform's
            // offsets produces, and it is what this test tolerated before those offsets came
            // from the interpreter's own headers.
            assert!(
                s.heap_size() >= MIN_TRACKED_OBJECTS,
                "sample {round}: gen {} entry {} reports heap_size {}, below the {} tracked \
                 objects the fixture holds at every Collection; the interpreter offsets look \
                 wrong for this build",
                s.generation,
                s.index,
                s.heap_size(),
                MIN_TRACKED_OBJECTS
            );
        }

        // Invariant 4: cumulative counters only ever grow.
        let heads = newest_per_gen(&written);
        let totals: Vec<i64> = heads.iter().map(|s| s.collections()).collect();
        assert_eq!(
            totals.len(),
            3,
            "sample {round}: only {} generations have Records; the fixture collects all three",
            totals.len()
        );
        if let Some(prev) = &prev_totals {
            for (g, (before, now)) in prev.iter().zip(totals.iter()).enumerate() {
                assert!(
                    now >= before,
                    "sample {round}: gen {g} collections went backwards: {before} -> {now}"
                );
            }
        }
        first_gen0.get_or_insert(totals[0]);
        last_gen0 = totals[0];
        prev_totals = Some(totals);
    }

    // A region that never changes satisfies non-regression on its own, and that is what a
    // reader latched onto a stale copy would see. The fixture collects gen 0 every ~50 ms, so
    // the counter has to move across the sampling window.
    let first = first_gen0.expect("at least one sample");
    assert!(
        last_gen0 > first,
        "gen 0 collections did not advance across {SAMPLES} samples ({first} -> {last_gen0}); \
         the region looks frozen rather than live"
    );
}
