use std::fmt::Write;
use std::path::Path;
use anyhow::Result;

use super::collect::CollectedData;

// ── Dark theme colors ──────────────────────────────────────────
const BG: &str = "#1e1e2e";
const BG2: &str = "#181825";
const FG: &str = "#cdd6f4";
const FG_MUTED: &str = "#6c7086";
const GRAY: &str = "#585b70";
const GREEN: &str = "#00ff88";
const CYAN: &str = "#00ccff";
const MAGENTA: &str = "#ff00ff";
const AMBER: &str = "#ffaa00";

const FIELD_COLORS: &[&str] = &[
    "#ff6b6b", "#ffd93d", "#6bcb77", "#4d96ff",
    "#c084fc", "#f97316", "#22d3ee", "#fb923c",
];
#[allow(dead_code)]
const GC_FIELD_COLORS: &[&str] = &[
    "#ff00ff", "#ff69b4", "#dda0dd", "#ff1493",
    "#c71585",
];

// ── Field definitions for _Py_DebugOffsets ─────────────────────
pub struct FieldDef {
    pub offset: usize,
    pub name: &'static str,
    pub color: &'static str,
}

pub const RUNTIME_FIELDS: &[FieldDef] = &[
    FieldDef { offset: 0, name: "cookie[8]", color: FIELD_COLORS[0] },
    FieldDef { offset: 8, name: "version", color: FIELD_COLORS[1] },
    FieldDef { offset: 16, name: "free_threaded", color: FIELD_COLORS[2] },
    FieldDef { offset: 24, name: "runtime_state.size", color: FIELD_COLORS[3] },
    FieldDef { offset: 32, name: "runtime_state.finalizing", color: FIELD_COLORS[4] },
    FieldDef { offset: 40, name: "runtime_state.interpreters_head", color: FIELD_COLORS[5] },
    FieldDef { offset: 48, name: "interpreter_state.size", color: FIELD_COLORS[6] },
    FieldDef { offset: 56, name: "interpreter_state.id", color: FIELD_COLORS[7] },
    FieldDef { offset: 64, name: "interpreter_state.next", color: FIELD_COLORS[0] },
    FieldDef { offset: 72, name: "interpreter_state.threads_head", color: FIELD_COLORS[1] },
    FieldDef { offset: 80, name: "interpreter_state.threads_main", color: FIELD_COLORS[2] },
    FieldDef { offset: 88, name: "interpreter_state.gc", color: FIELD_COLORS[3] },
];

// ── Tree entries for _Py_DebugOffsets ─────────────────────────
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum TreeEntryKind {
    RawValue { offset: usize },
    Group,
    Derived,
    Layout { field_type: &'static str, field_offset: u32 },
}

#[derive(Debug, Clone, Copy)]
pub struct TreeEntry {
    pub depth: u8,
    pub label: &'static str,
    pub kind: TreeEntryKind,
}

/// Build the full tree of _Py_DebugOffsets.
///
/// The GC-state subtree is data-driven, not hardcoded:
/// - `gc_fields` are the actual `gc` sub-struct fields as `(name, absolute offset within
///   _Py_DebugOffsets)`, from `VersionedOffsets::gc_debug_fields()`. On 3.13/3.14 this is
///   just `size`/`collecting`; on ring-buffer builds it also has
///   `frame`/`generation_stats_size`/`generation_stats`.
/// - `slot_fields` are the per-slot `gc_generation_stats` fields as `(name, offset within
///   one slot)`, from the resolved `GcItemLayout` (so a `+inc` build shows its extended
///   fields). `None` when this build exposes no readable stats layout.
///
/// The derived `generation_stats` layout subtree (item_size, young/old slot groups) is
/// emitted only when a `generation_stats` field is present (ring-buffer builds).
pub fn debug_offsets_tree(
    gc_fields: &[(&'static str, u64)],
    slot_fields: Option<&[(&'static str, usize)]>,
) -> Vec<TreeEntry> {
    let mut e = Vec::new();

    // depth 0
    e.push(TreeEntry { depth: 0, label: "_Py_DebugOffsets", kind: TreeEntryKind::Group });

    // depth 1
    e.push(TreeEntry { depth: 1, label: "cookie[8]",          kind: TreeEntryKind::RawValue { offset: 0 } });
    e.push(TreeEntry { depth: 1, label: "version",            kind: TreeEntryKind::RawValue { offset: 8 } });
    e.push(TreeEntry { depth: 1, label: "free_threaded",      kind: TreeEntryKind::RawValue { offset: 16 } });
    e.push(TreeEntry { depth: 1, label: "runtime_state",      kind: TreeEntryKind::Group });
    // depth 2 under runtime_state
    e.push(TreeEntry { depth: 2, label: "size",               kind: TreeEntryKind::RawValue { offset: 24 } });
    e.push(TreeEntry { depth: 2, label: "finalizing",         kind: TreeEntryKind::RawValue { offset: 32 } });
    e.push(TreeEntry { depth: 2, label: "interpreters_head",  kind: TreeEntryKind::RawValue { offset: 40 } });

    e.push(TreeEntry { depth: 1, label: "interpreter_state",  kind: TreeEntryKind::Group });
    // depth 2 under interpreter_state
    e.push(TreeEntry { depth: 2, label: "size",               kind: TreeEntryKind::RawValue { offset: 48 } });
    e.push(TreeEntry { depth: 2, label: "id",                 kind: TreeEntryKind::RawValue { offset: 56 } });
    e.push(TreeEntry { depth: 2, label: "next",               kind: TreeEntryKind::RawValue { offset: 64 } });
    e.push(TreeEntry { depth: 2, label: "threads_head",       kind: TreeEntryKind::RawValue { offset: 72 } });
    e.push(TreeEntry { depth: 2, label: "threads_main",       kind: TreeEntryKind::RawValue { offset: 80 } });
    e.push(TreeEntry { depth: 2, label: "gc",                 kind: TreeEntryKind::RawValue { offset: 88 } });

    // depth 3 under gc: actual gc sub-struct fields at their real offsets.
    for &(name, offset) in gc_fields {
        e.push(TreeEntry { depth: 3, label: name, kind: TreeEntryKind::RawValue { offset: offset as usize } });
    }

    // Ring-buffer builds publish a `generation_stats` pointer; only then does the
    // derived per-generation slot layout apply. Inline (3.13/3.14) and stat-less builds
    // have no such subtree.
    if gc_fields.iter().any(|&(name, _)| name == "generation_stats") {
        // depth 4 derived entries under generation_stats
        e.push(TreeEntry { depth: 4, label: "item_size",          kind: TreeEntryKind::Derived });
        e.push(TreeEntry { depth: 4, label: "young_slots (11)",   kind: TreeEntryKind::Derived });

        // depth 5 slot group under young_slots
        e.push(TreeEntry { depth: 5, label: "slot",               kind: TreeEntryKind::Group });

        // depth 6 actual slot fields
        if let Some(fields) = slot_fields {
            for &(name, off) in fields {
                e.push(TreeEntry { depth: 6, label: name, kind: TreeEntryKind::Layout { field_type: "", field_offset: off as u32 } });
            }
        }

        // depth 4 more derived entries
        e.push(TreeEntry { depth: 4, label: "index0",             kind: TreeEntryKind::Derived });
        e.push(TreeEntry { depth: 4, label: "old0_slots (3)",     kind: TreeEntryKind::Derived });
        e.push(TreeEntry { depth: 4, label: "index1",             kind: TreeEntryKind::Derived });
        e.push(TreeEntry { depth: 4, label: "old1_slots (3)",     kind: TreeEntryKind::Derived });
        e.push(TreeEntry { depth: 4, label: "index2",             kind: TreeEntryKind::Derived });
    }

    e
}

/// Compute tree prefix strings (ASCII only: +--, \--, |  )
pub fn tree_prefixes(entries: &[TreeEntry]) -> Vec<String> {
    fn has_sibling_after(entries: &[TreeEntry], i: usize, depth: u8) -> bool {
        entries[i + 1..].iter().any(|e| e.depth == depth)
    }

    let mut prefixes = Vec::with_capacity(entries.len());
    for (i, e) in entries.iter().enumerate() {
        let mut prefix = String::new();
        for d in 1..e.depth {
            if has_sibling_after(entries, i, d) {
                prefix.push_str("|   ");
            } else {
                prefix.push_str("    ");
            }
        }
        if e.depth > 0 {
            if has_sibling_after(entries, i, e.depth) {
                prefix.push_str("+-- ");
            } else {
                prefix.push_str("+-- ");
            }
        }
        prefixes.push(prefix);
    }
    prefixes
}

/// Compute gen_stats layout values
pub fn gen_stats_layout(gen_stats_size: u64) -> (u64, u64, u64, u64, u64, u64, u64) {
    let item_size = if gen_stats_size >= 24 { (gen_stats_size - 24) / 17 } else { 0 };
    let young_bytes = 11 * item_size;
    let old_bytes = 3 * item_size;
    let index0_off = young_bytes;
    let old0_off = index0_off + 8;
    let index1_off = old0_off + old_bytes;
    let old1_off = index1_off + 8;
    let index2_off = old1_off + old_bytes;
    (item_size, young_bytes, old_bytes, index0_off, index1_off, index2_off, old0_off)
}

// ── Public API ───────────────────────────────────────────────────────
pub fn render_svg(data: &CollectedData, output: &Path) -> Result<()> {
    let svg = build_svg(data);
    std::fs::write(output, svg.as_bytes())?;
    Ok(())
}

fn build_svg(data: &CollectedData) -> String {
    let mut s = String::new();

    writeln!(s, r#"<?xml version="1.0" encoding="UTF-8"?>"#).unwrap();
    writeln!(s, r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1400 1200" width="100%" height="100%">"#).unwrap();

    write!(s, r##"<style>
.mono {{ font-family: 'Cascadia Code','Fira Code','Consolas',monospace; font-size: 11px; }}
.t-hdr {{ fill: {}; font-size: 16px; font-weight: bold; font-family: 'Segoe UI','Helvetica Neue',sans-serif; }}
.t-title {{ fill: {}; font-size: 13px; font-weight: bold; font-family: 'Segoe UI','Helvetica Neue',sans-serif; }}
.t-label {{ fill: {}; font-size: 11px; font-family: 'Segoe UI','Helvetica Neue',sans-serif; }}
.t-val  {{ fill: {}; font-size: 10px; font-family: 'Cascadia Code','Fira Code','Consolas',monospace; }}
.t-fld  {{ fill: {}; font-size: 10px; font-family: 'Cascadia Code','Fira Code','Consolas',monospace; }}
.t-muted {{ fill: {}; font-size: 10px; font-family: 'Cascadia Code','Fira Code','Consolas',monospace; }}
</style>"##, FG, GREEN, FG, FG, FG_MUTED, GRAY).unwrap();

    writeln!(s, r#"<defs><marker id="arrow" viewBox="0 0 10 10" refX="5" refY="5" markerWidth="6" markerHeight="6" orient="auto"><path d="M0,0 L10,5 L0,10 Z" fill="{}"/></marker></defs>"#, AMBER).unwrap();

    writeln!(s, r#"<rect width="1400" height="1200" fill="{}"/>"#, BG).unwrap();

    // header
    writeln!(s, r#"<rect x="0" y="0" width="1400" height="48" fill="{}"/>"#, BG2).unwrap();
    writeln!(s, r#"<text x="20" y="28" class="t-hdr">gcscope diagram — PID {} — Python 0x{:08x}</text>"#,
        data.pid, data.runtime_version).unwrap();
    writeln!(s, r#"<text x="20" y="43" class="t-muted">_PyRuntime (aka _Py_DebugOffsets) @ {:#018x}  |  interpreter head @ {:#018x}</text>"#,
        data.runtime_addr, data.interpreter.addr).unwrap();

    let mut y = 60.0;
    y = render_section_runtime(&mut s, data, y);
    y = render_section_interpreter(&mut s, data, y);
    render_section_gc_stats(&mut s, data, y);

    writeln!(s, "</svg>").unwrap();
    s
}

// ── Section: _Py_DebugOffsets (/ _PyRuntime) ────────────────────────
fn render_section_runtime(s: &mut String, data: &CollectedData, y: f64) -> f64 {
    let table_h = RUNTIME_FIELDS.len() as f64 * 20.0;   // 12 fields = 240px
    let hex_h = 128.0 / 16.0 * 20.0;                     // 8 rows = 160px
    let h = table_h.max(hex_h) + 55.0;                   // header + padding
    let hdr = format!("_Py_DebugOffsets (embedded in _PyRuntime) @ {:#018x}", data.runtime_addr);

    writeln!(s, r##"<rect x="20" y="{}" width="1360" height="{}" rx="6" fill="#00ff8810" stroke="{}" stroke-width="1.5"/>"##, y, h, GREEN).unwrap();
    writeln!(s, r#"<text x="35" y="{}" class="t-title" fill="{}">{}</text>"#, y + 22.0, GREEN, hdr).unwrap();
    writeln!(s, r#"<line x1="35" y1="{}" x2="1370" y2="{}" stroke="{}" stroke-width="0.5"/>"#, y + 30.0, y + 30.0, GREEN).unwrap();

    render_field_table(s, 35.0, y + 42.0, RUNTIME_FIELDS, &data.runtime_raw_bytes, 128);
    render_hex_dump(s, 510.0, y + 42.0, &data.runtime_raw_bytes, 128, RUNTIME_FIELDS);

    y + h + 20.0
}

// ── Section: PyInterpreterState ──────────────────────────────────────
fn render_section_interpreter(s: &mut String, data: &CollectedData, y: f64) -> f64 {
    let interp = &data.interpreter;
    let off = data.offsets();

    // Calculate heights first
    let offset_list_rows = data.runtime_offset_fields().len();
    let offset_list_h = 18.0 + offset_list_rows as f64 * 20.0;
    let gc_box_h = 100.0;
    let left_h = offset_list_h + 5.0 + gc_box_h;

    // Show GC struct bytes from offset 128, covering generation_stats ptr (@136) and collecting (@144)
    let hex_offset: usize = 128;
    let hex_end = interp.gc.raw_bytes.len().min(hex_offset + 64);
    let hex_data = if hex_end > hex_offset { &interp.gc.raw_bytes[hex_offset..hex_end] } else { &[] };
    let hex_rows = (hex_data.len() + 15) / 16;
    let hex_h = hex_rows as f64 * 20.0;
    let right_h = 14.0 + 10.0 + hex_h + 10.0;
    let h = left_h.max(right_h) + 55.0;

    let hdr = format!("PyInterpreterState @ {:#018x}  (struct: {} bytes)", interp.addr, off.interpreter_state_size());

    writeln!(s, r##"<rect x="20" y="{}" width="1360" height="{}" rx="6" fill="#00ccff10" stroke="{}" stroke-width="1.5"/>"##, y, h, CYAN).unwrap();
    writeln!(s, r#"<text x="35" y="{}" class="t-title" fill="{}">{}</text>"#, y + 22.0, CYAN, hdr).unwrap();
    writeln!(s, r#"<line x1="35" y1="{}" x2="1370" y2="{}" stroke="{}" stroke-width="0.5"/>"#, y + 30.0, y + 30.0, CYAN).unwrap();

    // Left column: offset value table (table style matching runtime section)
    let table_title_y = y + 42.0;
    writeln!(s, r#"<text x="35" y="{}" class="t-label" fill="{}">Key offset values stored in _Py_DebugOffsets:</text>"#, table_title_y, CYAN).unwrap();
    let table_body_y = table_title_y + 18.0;
    render_offset_table(s, 35.0, table_body_y, &data.runtime_offset_fields());

    // GC sub-state box (left) — positioned below the table
    let gc_box_y = table_body_y + offset_list_rows as f64 * 20.0 + 5.0;
    let gc_addr = interp.addr + interp.gc_offset;
    writeln!(s, r##"<rect x="35" y="{}" width="450" height="{}" rx="4" fill="#ff00ff10" stroke="{}" stroke-width="1"/>"##, gc_box_y, gc_box_h, MAGENTA).unwrap();
    writeln!(s, r#"<text x="42" y="{}" class="t-label" fill="{}">GC State @ {:#018x}</text>"#,
        gc_box_y + 16.0, MAGENTA, gc_addr).unwrap();
    let mut gcy = gc_box_y + 30.0;

    let collecting_off = off.gc_collecting() as usize;
    let collecting_val = interp.gc.raw_bytes.get(collecting_off).copied().unwrap_or(0);
    writeln!(s, r##"<text x="42" y="{}" class="t-fld" fill="{}">  {:<38} {}</text>"##, gcy, MAGENTA, format!("collecting (u8 @ gc+{})", collecting_off), collecting_val).unwrap();
    gcy += 18.0;

    let gen_stats_off = off.gc_generation_stats() as usize;
    let gen_stats_ptr = if gen_stats_off + 8 <= interp.gc.raw_bytes.len() {
        u64::from_le_bytes(interp.gc.raw_bytes[gen_stats_off..gen_stats_off + 8].try_into().unwrap())
    } else { 0 };
    writeln!(s, r##"<text x="42" y="{}" class="t-fld" fill="{}">  {:<38} {}</text>"##, gcy, AMBER, format!("generation_stats (ptr @ gc+{})", gen_stats_off),
        if gen_stats_ptr != 0 { format!("{:#018x}", gen_stats_ptr) } else { "NULL".into() }).unwrap();
    gcy += 18.0;

    writeln!(s, r##"<text x="42" y="{}" class="t-fld" fill="{}">  {:<38} {}</text>"##, gcy, FG, "generation_stats_size (stored)", off.gc_generation_stats_size()).unwrap();

    // Right column: GC hex dump, aligned with GC box top
    let gc_hex_y = gc_box_y;
    writeln!(s, r#"<text x="510" y="{}" class="t-label" fill="{}">GC struct ({} bytes), bytes {}-{}</text>"#,
        gc_hex_y + 14.0, MAGENTA, interp.gc_size, hex_offset, hex_end.saturating_sub(1)).unwrap();
    render_hex_dump_simple(s, 510.0, gc_hex_y + 24.0, hex_data, hex_data.len());

    y + h + 20.0
}

// ── Section: GC Generation Stats ─────────────────────────────────────
fn render_section_gc_stats(s: &mut String, data: &CollectedData, y: f64) {
    let gc = &data.interpreter.gc.generation_stats;
    let h = if gc.slots.is_empty() { 160.0 } else { 380.0 };
    let hdr = if gc.stats_addr != 0 {
        format!("GC Generation Stats Buffer @ {:#018x} (size: {} bytes)", gc.stats_addr, gc.stats_size)
    } else {
        "GC Generation Stats: not available".to_string()
    };

    writeln!(s, r##"<rect x="20" y="{}" width="1360" height="{}" rx="6" fill="#ffaa0010" stroke="{}" stroke-width="1.5"/>"##, y, h, AMBER).unwrap();
    writeln!(s, r#"<text x="35" y="{}" class="t-title" fill="{}">{}</text>"#, y + 22.0, AMBER, hdr).unwrap();
    writeln!(s, r#"<line x1="35" y1="{}" x2="1370" y2="{}" stroke="{}" stroke-width="0.5"/>"#, y + 30.0, y + 30.0, AMBER).unwrap();

    if gc.slots.is_empty() || gc.raw_stats_bytes.is_empty() {
        writeln!(s, r#"<text x="35" y="{}" class="t-label" fill="{}">No GC stats available.</text>"#, y + 60.0, FG_MUTED).unwrap();
        return;
    }

    // stats table (left)
    let mut ty = y + 42.0;
    writeln!(s, r#"<text x="35" y="{}" class="t-label" fill="{}">Gen 0 (Young) — {} slots</text>"#, ty, AMBER, 11).unwrap();
    ty += 18.0;
    writeln!(s, r##"<text x="35" y="{}" class="t-muted">slot │ collected   candidates   heap_size      duration</text>"##, ty).unwrap();
    ty += 14.0;
    writeln!(s, r#"<line x1="35" y1="{}" x2="480" y2="{}" stroke="{}" stroke-width="0.5"/>"#, ty - 2.0, ty - 2.0, GRAY).unwrap();

    for slot in &gc.slots {
        if slot.generation != 0 { continue; }
        let heap_fmt = fmt_bytes(slot.heap_size as u64);
        writeln!(s, r##"<text x="35" y="{}" class="t-fld" fill="{}">{:>2}    │{:>10}  {:>10}  {:>10}  {:>8.3}ms</text>"##,
            ty, FG, slot.slot, slot.collected, slot.candidates, heap_fmt, slot.duration * 1000.0).unwrap();
        ty += 14.0;
    }

    ty += 4.0;
    writeln!(s, r#"<text x="35" y="{}" class="t-label" fill="{}">Gen 1 (Middle) — 3 slots  &amp;  Gen 2 (Oldest) — 3 slots</text>"#, ty, AMBER).unwrap();
    ty += 18.0;

    for slot in &gc.slots {
        if slot.generation == 0 { continue; }
        let gen_label = if slot.generation == 1 { "1(o0)" } else { "2(o1)" };
        let heap_fmt = fmt_bytes(slot.heap_size as u64);
        writeln!(s, r##"<text x="35" y="{}" class="t-fld" fill="{}">{}({:>2}) │{:>10}  {:>10}  {:>10}  {:>8.3}ms</text>"##,
            ty, FG, gen_label, slot.slot, slot.collected, slot.candidates, heap_fmt, slot.duration * 1000.0).unwrap();
        ty += 14.0;
    }

    // hex dump of first slot
    let item_size = if gc.stats_size > 24 && gc.stats_size < 10000 {
        ((gc.stats_size - 24) / 17) as usize
    } else {
        gc.raw_stats_bytes.len().min(64)
    };
    writeln!(s, r#"<text x="510" y="{}" class="t-label" fill="{}">First slot of stats buffer (~{item_size} bytes/slot):</text>"#,
        y + 42.0, AMBER).unwrap();

    let display_bytes = item_size.min(64);
    let hex_y = y + 56.0;
    render_hex_dump_simple(s, 510.0, hex_y, &gc.raw_stats_bytes, display_bytes);

    // slot field labels
    let slot_field_names = ["ts_start", "ts_stop", "collections", "collected",
                            "uncollectable", "candidates", "duration", "heap_size"];
    let label_x = 510.0 + 160.0 + 2.0 * 18.0 * 8.0 + 14.0;
    let mut ly = hex_y;
    for (i, name) in slot_field_names.iter().enumerate() {
        if i * 8 >= display_bytes { break; }
        writeln!(s, r#"<text x="{}" y="{}" class="t-muted">  ← {}</text>"#, label_x, ly + 11.0, name).unwrap();
        ly += 20.0;
    }
}

// ── Rendering helpers ────────────────────────────────────────────────
fn render_offset_table(s: &mut String, x: f64, y: f64, fields: &[super::collect::DebugOffsetField]) {
    let mut cy = y;
    for (i, f) in fields.iter().enumerate() {
        let color = FIELD_COLORS[i % FIELD_COLORS.len()];
        let fmt = if f.value > 0x10000 { format!("{:#x}", f.value) } else { format!("{}", f.value) };
        writeln!(s, r#"<rect x="{}" y="{}" width="8" height="8" rx="2" fill="{}"/>"#, x, cy + 3.0, color).unwrap();
        writeln!(s, r##"<text x="{}" y="{}" class="t-fld" fill="{}">{}</text>"##, x + 14.0, cy + 11.0, FG, f.name).unwrap();
        writeln!(s, r##"<text x="{}" y="{}" class="t-val" fill="{}">{}</text>"##, x + 310.0, cy + 11.0, color, fmt).unwrap();
        cy += 20.0;
    }
}

fn render_field_table(s: &mut String, x: f64, y: f64, fields: &[FieldDef], bytes: &[u8], limit: usize) {
    let mut cy = y;
    for field in fields {
        if field.offset + 8 > bytes.len() || field.offset + 8 > limit { continue; }
        let val_bytes: [u8; 8] = bytes[field.offset..field.offset + 8].try_into().unwrap();
        let val = u64::from_le_bytes(val_bytes);
        let fmt = fmt_u64(val, field.name);

        writeln!(s, r#"<rect x="{}" y="{}" width="8" height="8" rx="2" fill="{}"/>"#, x, cy + 3.0, field.color).unwrap();
        writeln!(s, r##"<text x="{}" y="{}" class="t-muted">{:#04x}</text>"##, x + 14.0, cy + 11.0, field.offset).unwrap();
        writeln!(s, r##"<text x="{}" y="{}" class="t-fld" fill="{}">{}</text>"##, x + 72.0, cy + 11.0, FG, field.name).unwrap();
        writeln!(s, r##"<text x="{}" y="{}" class="t-val" fill="{}">{}</text>"##, x + 310.0, cy + 11.0, field.color, fmt).unwrap();
        cy += 20.0;
    }
}

fn render_hex_dump(s: &mut String, x: f64, y: f64, bytes: &[u8], limit: usize, fields: &[FieldDef]) {
    let display = bytes.len().min(limit);
    let rows = (display + 15) / 16;
    let row_px = 20.0;

    for ri in 0..rows {
        let base = ri * 16;
        let row_end = (base + 16).min(display);
        let count = row_end - base;
        let ry = y + ri as f64 * row_px;

        writeln!(s, r##"<text x="{}" y="{}" class="t-muted">{:#018x}</text>"##, x, ry + 11.0, base).unwrap();

        let mut hx = x + 160.0;
        let mut ascii = String::new();

        for bi in 0..16 {
            if bi == 8 { hx += 6.0; }
            if bi < count {
                let boff = base + bi;
                let bv = bytes[boff];
                let color = byte_color(boff, fields);
                writeln!(s, r##"<text x="{}" y="{}" fill="{}" class="mono">{:02x}</text>"##, hx, ry + 11.0, color, bv).unwrap();
                let ch = if bv.is_ascii_graphic() || bv == b' ' { bv as char } else { '.' };
                ascii.push(ch);
            } else {
                writeln!(s, r##"<text x="{}" y="{}" fill="{}" class="mono">  </text>"##, hx, ry + 11.0, GRAY).unwrap();
            }
            hx += 18.0;
        }
        writeln!(s, r##"<text x="{}" y="{}" fill="{}" class="mono">│{}</text>"##, hx + 8.0, ry + 11.0, GRAY, ascii).unwrap();
    }
}

/// Hex dump without field coloring (uses neutral FG color)
fn render_hex_dump_simple(s: &mut String, x: f64, y: f64, bytes: &[u8], limit: usize) {
    let display = bytes.len().min(limit);
    let rows = (display + 15) / 16;
    let row_px = 20.0;

    for ri in 0..rows {
        let base = ri * 16;
        let row_end = (base + 16).min(display);
        let count = row_end - base;
        let ry = y + ri as f64 * row_px;

        writeln!(s, r##"<text x="{}" y="{}" class="t-muted">{:#018x}</text>"##, x, ry + 11.0, base).unwrap();

        let mut hx = x + 160.0;
        let mut ascii = String::new();

        for bi in 0..16 {
            if bi == 8 { hx += 6.0; }
            if bi < count {
                let bv = bytes[base + bi];
                writeln!(s, r##"<text x="{}" y="{}" fill="{}" class="mono">{:02x}</text>"##, hx, ry + 11.0, FG, bv).unwrap();
                let ch = if bv.is_ascii_graphic() || bv == b' ' { bv as char } else { '.' };
                ascii.push(ch);
            } else {
                writeln!(s, r##"<text x="{}" y="{}" fill="{}" class="mono">  </text>"##, hx, ry + 11.0, GRAY).unwrap();
            }
            hx += 18.0;
        }
        writeln!(s, r##"<text x="{}" y="{}" fill="{}" class="mono">│{}</text>"##, hx + 8.0, ry + 11.0, GRAY, ascii).unwrap();
    }
}

fn byte_color(offset: usize, fields: &[FieldDef]) -> &'static str {
    for field in fields {
        if offset >= field.offset && offset < field.offset + 8 {
            return field.color;
        }
    }
    GRAY
}

fn fmt_u64(val: u64, name: &str) -> String {
    if name.contains("cookie") {
        let bytes = val.to_le_bytes();
        let s = String::from_utf8_lossy(&bytes);
        format!("\"{}\"", s.trim_end_matches('\0'))
    } else if name.contains("version") {
        format!("0x{:08x}", val)
    } else if val > 0x10000 {
        format!("{:#018x}", val)
    } else {
        format!("{}", val)
    }
}

fn fmt_bytes(val: u64) -> String {
    if val >= 1000 * 1000 {
        format!("{:.1}M", val as f64 / (1000.0 * 1000.0))
    } else if val >= 1000 {
        format!("{:.1}K", val as f64 / 1000.0)
    } else {
        format!("{}", val)
    }
}
