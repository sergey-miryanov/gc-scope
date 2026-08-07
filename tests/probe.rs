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
//! The two tests that attach are `#[ignore]`d like the other live-attach tests (ADR 0005 §3);
//! `probe_module_exports_its_discovery_anchor` only reads the built module off disk and is not.
//! To run all three:
//!   cargo test --test probe -- --include-ignored
//! With no Probe installed each skips; see `common::probe_python`.

mod common;

use common::{
    SpawnedPython, full_python_version, is_free_threaded, probe_module_path, probe_python,
    probe_required,
};
use gcscope::memory::binary::{elf_load_bias, parse_macho};
use gcscope::memory::reader::{open_handle, read_memory_h};
use gcscope::memory::regions::list_regions;
use gcscope::remote_debugging::gc_stats::GcStat;
use gcscope::remote_debugging::offsets::offset_table::{
    GcItemLayout, GcStatsKind, OffsetTable, compute_ring_base_offsets,
};
use read_process_memory::ProcessHandle;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

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

/// The slot array. No reader looks this up, since the header publishes `slots_addr`, but it
/// carries the same export macro, and losing default visibility here is the same regression one
/// step earlier.
const SLOTS_SYMBOL: &str = "gcscope_probe_slots";

/// Samples across the run, and the gap between them. Three samples check a counter sequence
/// rather than a pair; the fixture collects gen 0 every ~50 ms, so this gap guarantees
/// progress without slowing the test.
const SAMPLES: usize = 3;
const SAMPLE_GAP: Duration = Duration::from_millis(700);

/// The sustained-churn window and its polling gap. Five seconds spans ~100 gen-0 Collections at
/// the fixture's ~50 ms cadence, ~20 gen-1 and ~4 gen-2. A 2 ms gap puts ~25 samples inside each
/// gen-0 Collection, so every Record is read several times over and an 11-entry young Ring
/// cannot lap between two samples.
///
/// A 2 ms gap still rarely catches the *publication* window. Writing a Record takes tens of
/// nanoseconds against a 50 ms cadence, so measured runs on Windows x86-64 sample ~2000 times
/// and observe zero entries mid-publication. Treat the in-flight checks below as guards on what
/// a reader could meet rather than as something this schedule produces.
const CHURN_WINDOW: Duration = Duration::from_secs(5);
const CHURN_GAP: Duration = Duration::from_millis(2);

/// Real counters stay far below this; garbage from a wrong address rarely does. Same bound as
/// the live-smoke shape check.
const SANE_COUNTER_MAX: i64 = 1_000_000_000_000; // 1e12

/// Floor for a written entry's `heap_size`, the count of GC-tracked objects the Probe snapshots
/// when a Collection starts.
///
/// The interpreter's own baseline holds this up, not the fixture's garbage. Only generation 0
/// snapshots the fresh burst: `probe_spin.py` collects generations 1 and 2 immediately after
/// `gc.collect(0)` has reclaimed it, so those entries see roughly what a started interpreter
/// tracks. Measured on 3.14 that is ~6800 for generations 1 and 2 against ~8500 for generation
/// 0. The floor sits a factor of five below both, and is not a number to raise on the
/// assumption that 6000 fresh objects are always in view.
///
/// Without that margin the check would catch nothing. Everything following `heap_size` in
/// `_gc_runtime_state` is a near-zero counter: `work_to_do` and `phase` on 3.14.4, the `dummy`
/// placeholders that replaced them on 3.14.5. An offset one field out reads a small number that
/// a bare positivity check waves through.
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
/// One goblin match covers PE, ELF and Mach-O, so every platform reads the same fact through
/// one lookup.
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
        // On macOS the kernel attributes several low no-access reservations to an image's
        // path, so its lowest mapping sits well below the Mach-O header. An address derived
        // from that base still lands inside *some* mapped range, which reads garbage
        // successfully instead of failing. The header sits at the start of `__TEXT`, the
        // executable mapping. gcscope's `find_python_modules` filters the same way.
        if cfg!(target_os = "macos") && !r.is_exec() {
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

    // ADR 0014 §2 names this failure: a module that works and is invisible to discovery. Say
    // which of the two happened.
    let addr = dynamic_symbol(&bytes, base, HEADER_SYMBOL)
        .map_err(|e| format!("{path}: {e}"))?
        .ok_or_else(|| {
            format!(
                "{path} does not export {HEADER_SYMBOL}; the module built but its discovery \
                 anchor is not in the table a remote reader consults"
            )
        })?;

    Ok((path, addr))
}

/// Resolve `symbol` in an image's dynamic symbol table, rebased onto `base`.
///
/// This reads every format through its dynamic table rather than its static one. `.symtab`, and
/// a Mach-O `nlist` marked private-extern, carry the symbol whatever its visibility, so falling
/// back to them would let a `-fvisibility=hidden` regression pass here while breaking discovery
/// in the field. gcscope's `resolve_symbol_elf` does fall back, for a different question. This
/// asks whether the symbol sits where a remote reader will look.
///
/// `Ok(None)` means the image parsed and does not export the symbol. `Err` means the image
/// could not be read as one of the three formats.
fn dynamic_symbol(bytes: &[u8], base: u64, symbol: &str) -> Result<Option<u64>, String> {
    match goblin::Object::parse(bytes) {
        Ok(goblin::Object::PE(pe)) => Ok(pe
            .exports
            .iter()
            .find(|e| e.name == Some(symbol))
            .map(|e| base + e.rva as u64)),
        Ok(goblin::Object::Elf(elf)) => {
            let bias = elf_load_bias(&elf)
                .ok_or_else(|| "no PT_LOAD segment to take a load bias from".to_string())?;
            Ok(elf
                .dynsyms
                .iter()
                .find(|s| elf.dynstrtab.get_at(s.st_name) == Some(symbol))
                .map(|s| base.wrapping_add(s.st_value.wrapping_sub(bias))))
        }
        Ok(goblin::Object::Mach(_)) => {
            // Through gcscope's own unwrapper: a Probe built against the universal2 Python
            // from python.org can itself be fat, and `MachO::parse` at offset 0 fails outright
            // on a fat header. Virtual addresses only below, so the slice offset goes unused.
            let (macho, _) = parse_macho(bytes)
                .ok_or_else(|| "Mach-O image did not parse for this architecture".to_string())?;
            // The export trie: what `-exported_symbols_list` and visibility control, so it
            // answers the same question `.dynsym` does above. Its offsets are relative to the
            // mach header, which is where `base` points on macOS once the caller has skipped
            // the non-executable reservations below `__TEXT`.
            let exports = macho
                .exports()
                .map_err(|e| format!("reading the Mach-O export trie: {e}"))?;
            // Mach-O prefixes C symbols with an underscore. Accept the undecorated spelling
            // too rather than assuming either form, as `resolve_symbol_macho` does.
            Ok(exports
                .iter()
                .find(|e| e.name == symbol || e.name.strip_prefix('_') == Some(symbol))
                .map(|e| base.wrapping_add(e.offset)))
        }
        Ok(other) => Err(format!(
            "parses as {}, which this lookup does not handle",
            object_kind(&other)
        )),
        Err(e) => Err(format!("parsing it: {e}")),
    }
}

/// Names the format goblin found, for the error above.
fn object_kind(obj: &goblin::Object) -> &'static str {
    match obj {
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

/// One generation's entries, split the way a reader consuming this Ring has to split them.
///
/// The writer publishes a Record by copying the previous entry forward, overwriting the
/// counters, and storing `ts_stop` last. Between those two acts the entry carries this
/// Collection's `ts_start` beside the previous occupant's `ts_stop`, and gcscope's
/// `is_complete()` calls it in-flight. A reader skips it and reports the newest entry that did
/// finish. The fields below are that split.
struct Head<'a> {
    /// The newest finished Collection: largest `collections`, since Records are cumulative and
    /// the Ring wraps.
    newest: Option<&'a GcStat>,
    /// Written but unfinished. At most one, the Record being published right now.
    in_flight: Vec<&'a GcStat>,
    /// Entries carrying a Collection at all. Unwritten ones are zeroed.
    written: usize,
}

fn head_of<'a>(stats: &'a [GcStat], generation: u32) -> Head<'a> {
    let written: Vec<&GcStat> = stats
        .iter()
        .filter(|s| s.generation == generation && s.collections() > 0)
        .collect();
    Head {
        newest: written
            .iter()
            .copied()
            .filter(|s| s.is_complete())
            .max_by_key(|s| s.collections()),
        in_flight: written
            .iter()
            .copied()
            .filter(|s| !s.is_complete())
            .collect(),
        written: written.len(),
    }
}

/// The unfinished entries, listed. Two callers want different counts around the same list, so
/// the count stays with each of them rather than being guessed here.
fn describe_in_flight(head: &Head<'_>) -> String {
    head.in_flight
        .iter()
        .map(|s| {
            format!(
                "entry {} ts_start={} ts_stop={}",
                s.index,
                s.ts_start(),
                s.ts_stop()
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The message for a generation whose every written entry reads as unfinished. One is the
/// publication window. All of them means `ts_stop` never lands after `ts_start`, so nothing is
/// writing the field or this is reading it at the wrong offset.
fn describe_all_in_flight(head: &Head<'_>, generation: u32) -> String {
    format!(
        "gen {generation}: all {} written entries read as unfinished ({})",
        head.written,
        describe_in_flight(head)
    )
}

/// What one sample took from a generation's newest finished Record, kept for the next sample to
/// compare against. Copied out rather than borrowed, since the buffer it decodes from is
/// replaced every round.
#[derive(Clone, Copy)]
struct Newest {
    index: usize,
    collections: i64,
    collected: i64,
    uncollectable: i64,
    duration: f64,
    ts_start: i64,
    ts_stop: i64,
}

impl Newest {
    fn of(s: &GcStat) -> Self {
        Newest {
            index: s.index,
            collections: s.collections(),
            collected: s.collected(),
            uncollectable: s.uncollectable(),
            duration: s.duration(),
            ts_start: s.ts_start(),
            ts_stop: s.ts_stop(),
        }
    }
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

/// An interpreter with a Probe, or `None` after logging a skip.
///
/// [`probe_python`] calls into the module rather than importing it: the source directory in the
/// crate root imports as a namespace package on any interpreter, so an import proves nothing.
/// `GCSCOPE_REQUIRE_PROBE=1` turns the skip into a failure, since a leg that just built a Probe
/// should find one.
fn probe_python_or_skip(test: &str) -> Option<PathBuf> {
    if let Some(python) = probe_python() {
        return Some(python);
    }
    let msg = "no interpreter with the `gcscope_probe` extension installed \
               (build it: pip install ./gcscope_probe)";
    assert!(
        !probe_required(),
        "GCSCOPE_REQUIRE_PROBE=1 but {msg}; a leg that builds a Probe must not pass by skipping"
    );
    eprintln!("SKIP {test}: {msg}");
    None
}

/// A running fixture with its Ring located: the preamble both live tests share.
struct Attached {
    /// Kept alive rather than read: dropping it kills the interpreter.
    _proc: SpawnedPython,
    module: String,
    handle: ProcessHandle,
    header: ProbeHeader,
    table: OffsetTable,
    /// Bytes to read per sample, from the geometry the target declared.
    region_len: usize,
    /// The first claimed interpreter slot: `(id, region address)`.
    iid: i64,
    addr: u64,
}

/// Spawn `probe_spin.py`, find its published header, and resolve the first claimed slot's Ring.
fn attach(python: &Path) -> Attached {
    // A free-threaded build never maintains `heap_size`, so the Probe refuses to load there
    // rather than publish a column of zeros. Reaching this assertion means it loaded anyway,
    // a broken gate rather than an unsupported configuration, so it fails instead of skipping.
    assert!(
        !is_free_threaded(python),
        "GCSCOPE_TEST_PYTHON selected a free-threaded build and the Probe imported anyway; \
         the Py_GIL_DISABLED gate in PyInit_gcscope_probe is not doing its job"
    );

    let proc = SpawnedPython::spawn_fixture(python, "probe_spin.py")
        .expect("probe_spin.py should reach READY");
    let pid = proc.pid();

    let (module, header_addr) = find_header_addr(pid).expect("locate the published header");
    let handle = open_handle(pid).expect("open the target process");
    let header = read_header(&handle, header_addr).expect("decode the published header");

    let regions = live_regions(&handle, &header).expect("walk the slot array");
    let (iid, addr) = *regions
        .first()
        .expect("no claimed interpreter slot; the callback never ran");

    let table = ring_table(&header);
    let region_len = table
        .stats_buffer_len()
        .expect("ring geometry is decodable");

    Attached {
        _proc: proc,
        module,
        handle,
        header,
        table,
        region_len,
        iid,
        addr,
    }
}

/// The discovery anchor sits in the table a remote reader consults.
///
/// This reads the built module off disk, with no ptrace and no fixture, so it goes red in any
/// job that builds a Probe rather than only where an attach is possible. ADR 0014 §2 calls that
/// failure silent: the module loads, installs its callback, publishes a valid region, and
/// exports nothing to find it by. `PyMODINIT_FUNC` carries default visibility on its own. These
/// two do not, so a build that picked up `-fvisibility=hidden` would import fine and vanish
/// from discovery.
///
/// It resolves through the same [`dynamic_symbol`] the live test locates the region with, so a
/// pass here covers the lookup discovery performs.
#[test]
fn probe_module_exports_its_discovery_anchor() {
    let Some(python) = probe_python_or_skip("probe_module_exports_its_discovery_anchor") else {
        return;
    };
    let path = probe_module_path(&python)
        .expect("an interpreter that imported the extension has it on disk");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));

    for symbol in [HEADER_SYMBOL, SLOTS_SYMBOL] {
        let found =
            dynamic_symbol(&bytes, 0, symbol).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(
            found.is_some(),
            "{} does not export {symbol} in its dynamic symbol table",
            path.display()
        );
    }

    // The negative control, without which the loop above passes on a lookup that answers Some
    // to everything. `gcscope_probe_records_written` is a file-scope `static` in
    // `gcscope_probe.c` and reaches no dynamic table on any of the three formats.
    let internal = dynamic_symbol(&bytes, 0, "gcscope_probe_records_written")
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(
        internal.is_none(),
        "{} exports gcscope_probe_records_written, which has internal linkage; the lookup is \
         answering from a table wider than the one a remote reader consults",
        path.display()
    );
}

/// The four invariants the prototype's verifier established, asserted against Records read
/// out of the process: every generation decodes an entry a reader can report, that entry is a
/// finished Collection, it carries a positive duration, and cumulative counters never regress
/// between samples. A fifth checks `heap_size`, the one field the Probe reaches by raw offset
/// into an internal struct.
///
/// The prototype demanded `is_complete()` of *every* written entry, which the Ring does not
/// owe a reader: the Record being published is torn by construction until its `ts_stop` lands.
/// See [`Head`]. `probe_ring_survives_sustained_churn` polls hard enough to be the test that
/// holds that window to one entry.
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport and an installed Probe — run with --ignored"]
fn probe_ring_decodes_out_of_process() {
    let Some(python) = probe_python_or_skip("probe_ring_decodes_out_of_process") else {
        return;
    };
    let t = attach(&python);
    let (h, module, handle) = (&t.header, &t.module, &t.handle);

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

    // gcscope's `stats_buffer_len` stops at the end of gen 2's entries, while the producer's
    // region also carries gen 2's trailing cursor word, making it 8 bytes longer. A superset
    // is correct; a shorter region would mean the two disagree about geometry.
    assert!(
        h.region_size as usize >= t.region_len,
        "declared region {} is smaller than its own geometry implies ({})",
        h.region_size,
        t.region_len
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
        let raw = read_memory_h(handle, t.addr, t.region_len)
            .unwrap_or_else(|e| panic!("sample {round}: reading the region: {e}"));
        let stats = t.table.decode_gc_stats(&raw, t.iid);

        if let Err(e) = check_shape(&stats, entries) {
            panic!("sample {round}: {e}");
        }

        let mut totals: Vec<i64> = Vec::with_capacity(3);
        for generation in 0..3u32 {
            let head = head_of(&stats, generation);
            assert!(
                head.written > 0,
                "sample {round}: gen {generation} carries no Record; the fixture collects all three"
            );
            // Invariants 1 and 2, as a reader applies them: only the entry being published now
            // may be unfinished, and the newest finished one gets reported. `Head` explains why
            // one incomplete entry is expected here, and
            // `probe_ring_survives_sustained_churn` bounds it to one.
            let s = head.newest.unwrap_or_else(|| {
                panic!(
                    "sample {round}: {}",
                    describe_all_in_flight(&head, generation)
                )
            });

            // Invariant 3. `duration` is cumulative, so a written entry carries some.
            assert!(
                s.duration() > 0.0,
                "sample {round}: gen {generation} entry {} has non-positive duration {}",
                s.index,
                s.duration()
            );
            // Invariant 5: `heap_size` came from the interpreter rather than from an offset
            // pointing elsewhere. It is the one field the Probe reaches by byte offset into a
            // struct with no accessor, so it is what ADR 0013's compiled-in offsets exist for.
            // A wheel carrying another platform's offsets reads 0 here, which this test
            // tolerated before those offsets came from the interpreter's own headers.
            assert!(
                s.heap_size() >= MIN_TRACKED_OBJECTS,
                "sample {round}: gen {generation} entry {} reports heap_size {}, below the {} tracked \
                 objects the fixture holds at every Collection; the interpreter offsets look \
                 wrong for this build",
                s.index,
                s.heap_size(),
                MIN_TRACKED_OBJECTS
            );
            totals.push(s.collections());
        }

        // Invariant 4: cumulative counters only ever grow.
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

/// Poll the Ring far faster than it is written and hold every sample to what a reader is
/// entitled to assume about the entry it picks as newest.
///
/// `specs/0013-probe-portable-core.md` §5 case 3 asks for this guard on publication ordering.
/// `gcscope_probe_add_stats` copies the previous entry forward, overwrites the counters and
/// publishes `ts_stop` with `memory_order_release`, so a reader that sees a Record's `ts_stop`
/// can expect the payload it describes. The three-sample test above crosses that boundary a
/// handful of times, too few to call it exercised.
///
/// On x86-64 a missing release store cannot turn this red. TSO orders the stores regardless, so
/// the Linux and Windows legs show the invariants hold without showing that the release holds
/// them. Only the `probe-contract` leg on `macos-latest` separates the two, running on native
/// Apple Silicon. Emulation could serialise the writes and mask the defect, so the workflow
/// asserts that leg is arm64 instead of trusting the label.
///
/// Two `ts_start` clauses below could fire on a reordering on any machine. They ask whether the
/// counters and the interval in the selected entry describe the same Collection, covering the
/// two directions those fields come apart in. Neither implies the other: an earlier version
/// carried only the second and passed on the more likely case.
///
/// None of this observes the publication window itself. A healthy run catches zero entries
/// mid-publication (see `CHURN_GAP`), so the `in_flight` assertions bound that state from above
/// and never witness it.
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport and an installed Probe — run with --ignored"]
fn probe_ring_survives_sustained_churn() {
    let Some(python) = probe_python_or_skip("probe_ring_survives_sustained_churn") else {
        return;
    };
    let t = attach(&python);

    let mut prev: [Option<Newest>; 3] = [None; 3];
    let mut first_gen0: Option<i64> = None;
    let mut last_gen0 = 0i64;
    let mut samples = 0usize;
    let mut in_flight_seen = 0usize;

    let deadline = Instant::now() + CHURN_WINDOW;
    while Instant::now() < deadline {
        thread::sleep(CHURN_GAP);
        samples += 1;

        let raw = read_memory_h(&t.handle, t.addr, t.region_len)
            .unwrap_or_else(|e| panic!("sample {samples}: reading the region: {e}"));
        let stats = t.table.decode_gc_stats(&raw, t.iid);

        for generation in 0..3u32 {
            let head = head_of(&stats, generation);
            assert!(
                head.written > 0,
                "sample {samples}: gen {generation} carries no Record; probe_spin.py seeds all three \
                 before it reports READY"
            );

            // The writer publishes one Record at a time, so one entry may be part-way through
            // and no more. A second would mean an entry left torn by something other than the
            // window this samples across.
            in_flight_seen += head.in_flight.len();
            assert!(
                head.in_flight.len() <= 1,
                "sample {samples}: gen {generation} has {} entries mid-publication at once ({}); \
                 the writer publishes one Record at a time, so at most one can be torn",
                head.in_flight.len(),
                describe_in_flight(&head)
            );

            let s = head.newest.unwrap_or_else(|| {
                panic!(
                    "sample {samples}: {}",
                    describe_all_in_flight(&head, generation)
                )
            });
            let now = Newest::of(s);

            // `Head::newest` already filtered on `is_complete()`, which today *is*
            // `ts_start < ts_stop`. Restated here so the invariant this test exists for is
            // asserted rather than inherited: gcscope owns that definition, and a reader is
            // entitled to this whatever it loosens to.
            assert!(
                s.ts_stop() > now.ts_start,
                "sample {samples}: gen {generation} entry {} is the newest finished Record and its \
                 ts_stop {} does not follow its ts_start {}",
                now.index,
                s.ts_stop(),
                now.ts_start
            );
            assert!(
                now.duration > 0.0,
                "sample {samples}: gen {generation} entry {} has non-positive duration {}",
                now.index,
                now.duration
            );
            assert!(
                s.heap_size() >= MIN_TRACKED_OBJECTS,
                "sample {samples}: gen {generation} entry {} reports heap_size {}, below the {} \
                 tracked objects the fixture holds at every Collection",
                now.index,
                s.heap_size(),
                MIN_TRACKED_OBJECTS
            );

            if let Some(before) = prev[generation as usize] {
                for (name, was, is) in [
                    ("collections", before.collections, now.collections),
                    ("collected", before.collected, now.collected),
                    ("uncollectable", before.uncollectable, now.uncollectable),
                ] {
                    assert!(
                        is >= was,
                        "sample {samples}: gen {generation} {name} went backwards: {was} -> {is}"
                    );
                }
                assert!(
                    now.duration >= before.duration,
                    "sample {samples}: gen {generation} duration went backwards: {} -> {}",
                    before.duration,
                    now.duration
                );
                // The counters and the timestamps have to describe the same Collection, and a
                // partly-visible entry is where they come apart. Two clauses, for the two
                // orders that can happen, since neither implies the other:
                //
                //  - `collections` moved but the clock did not. A generation cannot collect
                //    concurrently with itself, so Collection N starts no earlier than
                //    Collection N-1 stopped. An entry whose count advanced while its ts_start
                //    still sits before the previous entry's ts_stop is carrying one
                //    Collection's total beside another's interval.
                //  - The clock moved but `collections` did not: this Collection's start time
                //    over the previous one's totals. A partly-copied entry moves ts_start the
                //    other way, back to the occupant a full lap ago, so this asks only that it
                //    not run ahead.
                if now.collections > before.collections {
                    assert!(
                        now.ts_start >= before.ts_stop,
                        "sample {samples}: gen {generation} advanced to {} collections but \
                         starts at {}, before the {} its previous Record stopped at; the \
                         counters and the interval belong to different Collections",
                        now.collections,
                        now.ts_start,
                        before.ts_stop
                    );
                } else {
                    assert!(
                        now.ts_start <= before.ts_start,
                        "sample {samples}: gen {generation} still reports {} collections while \
                         its ts_start advanced {} -> {}; the Record was visible before its \
                         payload was",
                        now.collections,
                        before.ts_start,
                        now.ts_start
                    );
                }
            }

            if generation == 0 {
                first_gen0.get_or_insert(now.collections);
                last_gen0 = now.collections;
            }
            prev[generation as usize] = Some(now);
        }
    }

    // Both a floor on the polling this did and the frozen-region check: a reader
    // latched onto a stale copy satisfies every invariant above.
    let first = first_gen0.expect("the churn window fits at least one sample");
    assert!(
        samples > 100,
        "only {samples} samples fit in {CHURN_WINDOW:?}; too few to call the Ring churned"
    );
    assert!(
        last_gen0 > first,
        "gen 0 collections did not advance across {samples} samples ({first} -> {last_gen0}); \
         the region looks frozen rather than live"
    );
    eprintln!(
        "{samples} samples over {CHURN_WINDOW:?}, gen 0 collections {first} -> {last_gen0}, \
         {in_flight_seen} entries caught mid-publication ({})",
        t.module
    );
}
