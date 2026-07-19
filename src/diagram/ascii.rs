use std::fmt::Write;

use super::collect::CollectedData;

// Total line width: 160 chars including | borders.
// | (1) + left panel (PL) + " | " (3) + right panel (PR) + | (1) = 4 + PL + PR = 160
// => PL + PR = 156. Using PL = 65, PR = 90.

const OUTER_W: usize = 158; // dashes between outer +/+ for top/bot/sep
const CONTENT_W: usize = 156; // padded content width for l() between |/|
const PL: usize = 65;         // left panel content width (shorter)
const PR: usize = 90;         // right panel content width (wider)

fn top() -> String {
    format!("+{}+", "-".repeat(OUTER_W))
}
fn bot() -> String {
    format!("+{}+", "-".repeat(OUTER_W))
}
fn sep() -> String {
    format!("+{}+", "-".repeat(OUTER_W))
}

fn l(content: &str) -> String {
    format!("| {:<w$} |", content, w = CONTENT_W)
}

fn panels(left: &[String], right: &[String]) -> Vec<String> {
    let max = left.len().max(right.len());
    let mut out = Vec::with_capacity(max);
    for i in 0..max {
        let lv = left.get(i).map(|s| s.as_str()).unwrap_or("");
        let rv = right.get(i).map(|s| s.as_str()).unwrap_or("");
        out.push(format!("|{l:<pl$} | {r:<pr$}|", l = lv, r = rv, pl = PL, pr = PR));
    }
    out
}

pub fn render_ascii(data: &CollectedData, rate_per_gen: [Option<f64>; 3], avg_coll_time_per_gen: [Option<f64>; 3]) -> String {
    let mut s = String::new();
    writeln!(s, "{}", top()).unwrap();
    writeln!(s, "{}", l(&format!("gcscope ascii - PID {} - Python 0x{:08x}", data.pid, data.runtime_version))).unwrap();
    writeln!(s, "{}", l(&format!("_PyRuntime (aka _Py_DebugOffsets) @ {:#x}  |  Interpreter head @ {:#x}", data.runtime_addr, data.interpreter.addr))).unwrap();
    writeln!(s, "{}", bot()).unwrap();
    writeln!(s).unwrap();
    s = render_runtime(s, data);
    s = render_interpreter(s, data);
    s = render_gc_stats(s, data, rate_per_gen, avg_coll_time_per_gen);
    s
}

// -- Section: _Py_DebugOffsets --------------------------------
fn render_runtime(mut s: String, data: &CollectedData) -> String {
    let bytes = &data.runtime_raw_bytes;
    let debug_size = data.debug_offsets_size as usize;

    // `gc.generation_stats_size` is read from the target's `_Py_DebugOffsets`, so the
    // accessor already holds the process-published value (0 on builds without the field).
    let gen_stats_size = data.offsets().gc_generation_stats_size();
    let gs = super::render::gen_stats_layout(gen_stats_size);

    // Drive the GC-state subtree from actual, version-correct layout.
    let gc_fields = data.offsets().gc_debug_fields();
    let offset_table = data.offsets().to_offset_table(data.pid, data.runtime_addr);
    let slot_fields = offset_table.gc_layout.map(|l| l.fields);
    let tree = super::render::debug_offsets_tree(&gc_fields, slot_fields);
    let prefixes = super::render::tree_prefixes(&tree);

    // Helper to read a u64 from raw bytes
    let read_u64 = |off: usize| -> u64 {
        if off + 8 <= bytes.len() && off + 8 <= debug_size {
            u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap())
        } else {
            0
        }
    };

    let fmt_val = |val: u64, name: &str| -> String {
        if name.contains("cookie") {
            let b = val.to_le_bytes();
            let sv = String::from_utf8_lossy(&b);
            format!("\"{}\"", sv.trim_end_matches('\0'))
        } else if name.contains("version") {
            format!("0x{:08x}", val)
        } else if name.contains("size") {
            format!("{}", val)
        } else if val > 0xFFFF_FFFF {
            format!("{:#x}", val)
        } else if val > 0x10000 {
            format!("{} ({:#x})", val, val)
        } else {
            format!("{}", val)
        }
    };

    let mut left_lines: Vec<String> = Vec::new();
    left_lines.push(format!("{:<pl$}", "Fields:", pl = PL));

    for (i, entry) in tree.iter().enumerate() {
        let pfx = &prefixes[i];
        let line = match entry.kind {
            super::render::TreeEntryKind::RawValue { offset } => {
                let val = read_u64(offset);
                let f = fmt_val(val, entry.label);
                format_tree_line(pfx, &format!("0x{:04x}  ", offset), entry.label, &f)
            }
            super::render::TreeEntryKind::Group => {
                format_tree_line(pfx, "", entry.label, "")
            }
            super::render::TreeEntryKind::Derived => {
                let val_str = derived_val(entry.label, gen_stats_size, gs);
                format_tree_line(pfx, "", entry.label, &val_str)
            }
            super::render::TreeEntryKind::Layout { field_type: _, field_offset } => {
                let val_str = format!("+{}", field_offset);
                format_tree_line(pfx, "", entry.label, &val_str)
            }
        };
        left_lines.push(line);
    }

    let hex_slice = &bytes[..debug_size.min(bytes.len())];
    let hex_end = debug_size.saturating_sub(1);
    let mut hex_lines = vec![format!("{:<pr$}",
        &format!("Hex Dump (DebugOffsets, 0x0000-0x{:04x}, {} bytes):", hex_end, debug_size),
        pr = PR)];
    hex_lines.extend(hex_panel(hex_slice, hex_slice.len(), 0, PR));

    let body = panels(&left_lines, &hex_lines);

    writeln!(s, "{}", top()).unwrap();
    writeln!(s, "{}", l(&format!("_Py_DebugOffsets (embedded in _PyRuntime) @ {:#x}  (size: {} bytes)", data.runtime_addr, debug_size))).unwrap();
    writeln!(s, "{}", sep()).unwrap();
    for line in &body {
        writeln!(s, "{}", line).unwrap();
    }
    writeln!(s, "{}", bot()).unwrap();
    writeln!(s).unwrap();
    s
}

fn format_tree_line(prefix: &str, offset_str: &str, name: &str, value_str: &str) -> String {
    let before = format!("{}{}{}", prefix, offset_str, name);
    let total = PL;
    let pad = total.saturating_sub(before.len() + value_str.len());
    format!("{}{}{}", before, " ".repeat(pad), value_str)
}

#[allow(unused_variables)]
fn derived_val(label: &str, gs_size: u64, gs: (u64, u64, u64, u64, u64, u64, u64)) -> String {
    let (item_size, young_bytes, _old_bytes, index0_off, index1_off, index2_off, _old0_off) = gs;
    match label {
        "item_size" => format!("{}", item_size),
        "young_slots (11)" => format!("11 x {} = {} bytes", item_size, young_bytes),
        "index0" => format!("+{}", index0_off),
        "old0_slots (3)" => format!("3 x {} bytes", item_size),
        "index1" => format!("+{}", index1_off),
        "old1_slots (3)" => format!("3 x {} bytes", item_size),
        "index2" => format!("+{}", index2_off),
        _ => String::new(),
    }
}

// -- Section: PyInterpreterState ------------------------------
fn render_interpreter(mut s: String, data: &CollectedData) -> String {
    let interp = &data.interpreter;
    let off = data.offsets();
    // Show the whole GC state struct (raw_bytes is read to exactly gc.size bytes), so the
    // dump matches the "GC struct (N bytes)" header. A fixed cap truncated larger structs
    // like the +inc build's 232-byte state.
    let hex_end = interp.gc.raw_bytes.len();

    let mut left_lines: Vec<String> = Vec::new();
    left_lines.push(format!("{:<pl$}", "Key offset values stored in _Py_DebugOffsets:", pl = PL));
    for f in data.runtime_offset_fields() {
        if f.name.starts_with("runtime_state") || f.name.starts_with("gc") { continue; }
        let v = fmt_val(f.value);
        left_lines.push(format!("    {:<30}  {:>18}", f.name, v));
    }

    // inner box for GC state - shows all GC fields with type/offset/hex/decimal
    let inner_w = PL - 4;
    let gc_addr = interp.addr + interp.gc_offset;
    left_lines.push(format!("{:<pl$}", "", pl = PL));
    left_lines.push(format!("  +{}+", "-".repeat(inner_w)));
    left_lines.push(format!("  | {:<tw$} |", format!("GC State @ {:#x}", gc_addr), tw = inner_w - 2));
    left_lines.push(format!("  +{}+", "-".repeat(inner_w)));

    // The descriptor `gc` sub-struct is append-only and shorter on older builds (2 fields on
    // 3.13/3.14, all 5 on 3.15+), so list only the fields this version actually publishes —
    // otherwise absent fields render as phantom `@ gc+0` / NULL rows. `gc_debug_fields()` is
    // the same version-correct source Section 1 uses.
    let present: Vec<&'static str> = off.gc_debug_fields().into_iter().map(|(n, _)| n).collect();

    // 1. size
    if present.contains(&"size") {
        let line = format!("  {:<15} (store)    {}", "size", interp.gc_size);
        left_lines.push(format!("  | {:<tw$} |", line, tw = inner_w - 2));
    }

    // 2. collecting
    let collecting_off = off.gc_collecting() as usize;
    let collecting_val = interp.gc.raw_bytes.get(collecting_off).copied().unwrap_or(0);
    if present.contains(&"collecting") {
        let line = format!("  {:<15} @ gc+{:<4}  {}", "collecting", collecting_off, collecting_val);
        left_lines.push(format!("  | {:<tw$} |", line, tw = inner_w - 2));
    }

    // 3. frame
    let frame_off = off.gc_frame() as usize;
    let frame_val = if frame_off + 8 <= interp.gc.raw_bytes.len() {
        u64::from_le_bytes(interp.gc.raw_bytes[frame_off..frame_off + 8].try_into().unwrap())
    } else { 0 };
    if present.contains(&"frame") {
        let line = format!("  {:<15} @ gc+{:<4}  {:#x}", "frame", frame_off, frame_val);
        left_lines.push(format!("  | {:<tw$} |", line, tw = inner_w - 2));
    }

    // 4. generation_stats_size
    if present.contains(&"generation_stats_size") {
        let line = format!("  {:<15} (store)    {}", "gen_stats_size", off.gc_generation_stats_size());
        left_lines.push(format!("  | {:<tw$} |", line, tw = inner_w - 2));
    }

    // 5. generation_stats
    let gen_stats_off = off.gc_generation_stats() as usize;
    let gen_stats_ptr = if gen_stats_off + 8 <= interp.gc.raw_bytes.len() {
        u64::from_le_bytes(interp.gc.raw_bytes[gen_stats_off..gen_stats_off + 8].try_into().unwrap())
    } else { 0 };
    let ptr_str = if gen_stats_ptr != 0 { format!("{:#x}", gen_stats_ptr) } else { "NULL".into() };
    if present.contains(&"generation_stats") {
        let line = format!("  {:<15} @ gc+{:<4}  {}", "gen_stats", gen_stats_off, ptr_str);
        left_lines.push(format!("  | {:<tw$} |", line, tw = inner_w - 2));
    }

    left_lines.push(format!("  +{}+", "-".repeat(inner_w)));

    let mut right_lines = vec![format!("{:<pr$}", format!("GC struct ({} bytes) hex dump:", interp.gc_size), pr = PR)];
    right_lines.extend(hex_panel(&interp.gc.raw_bytes, hex_end, 0, PR));

    let body = panels(&left_lines, &right_lines);

    writeln!(s, "{}", top()).unwrap();
    writeln!(s, "{}", l(&format!("PyInterpreterState @ {:#x}  (struct: {} bytes)", interp.addr, off.interpreter_state_size()))).unwrap();
    writeln!(s, "{}", sep()).unwrap();
    for line in &body {
        writeln!(s, "{}", line).unwrap();
    }
    writeln!(s, "{}", bot()).unwrap();
    writeln!(s).unwrap();
    s
}

// -- Section: GC Generation Stats ----------------------------
fn render_gc_stats(mut s: String, data: &CollectedData, rate_per_gen: [Option<f64>; 3], avg_coll_time_per_gen: [Option<f64>; 3]) -> String {
    let gc = &data.interpreter.gc.generation_stats;

    let mut left_lines: Vec<String> = Vec::new();
    let mut right_lines: Vec<String> = Vec::new();

    if gc.stats_addr == 0 || gc.slots.is_empty() {
        left_lines.push("No GC stats available.".into());
        let body = panels(&left_lines, &right_lines);
        writeln!(s, "{}", top()).unwrap();
        writeln!(s, "{}", l("GC Generation Stats: not available")).unwrap();
        writeln!(s, "{}", sep()).unwrap();
        for line in &body {
            writeln!(s, "{}", line).unwrap();
        }
        writeln!(s, "{}", bot()).unwrap();
        writeln!(s).unwrap();
        return s;
    }

    // Version-correct geometry/layout for this build (IO-free): drives the per-slot size,
    // the per-generation slot counts, and the slot-items field list below.
    let offset_table = data.offsets().to_offset_table(data.pid, data.runtime_addr);
    let item_size = if gc.item_size > 0 { gc.item_size } else { gc.raw_stats_bytes.len().min(64) };
    // Per-generation slot counts come from the collected snapshot (version/layout-derived,
    // FT-correct) rather than a GIL-assuming literal or a per-frame tally.
    let slots_per_gen = gc.slots_per_gen;

    let gen_names = [
        (format!("Gen 0 (Young) - {} slots", slots_per_gen[0]), rate_per_gen[0], avg_coll_time_per_gen[0]),
        (format!("Gen 1 (Middle) - {} slots", slots_per_gen[1]), rate_per_gen[1], avg_coll_time_per_gen[1]),
        (format!("Gen 2 (Oldest) - {} slots", slots_per_gen[2]), rate_per_gen[2], avg_coll_time_per_gen[2]),
    ];
    for (name, rate, avg_coll) in &gen_names {
        let rate_str = match rate { Some(r) => fmt_rate(*r), None => "n/a".to_string() };
        let coll_str = match avg_coll { Some(d) => fmt_duration(*d), None => "n/a".to_string() };
        left_lines.push(format!("{:<pl$}", format!("{}  (rate = {}, avg coll = {})", name, rate_str, coll_str), pl = PL));
    }

    left_lines.push(format!("{:<pl$}", format!("slot size: {} bytes  |  total buffer: {} bytes", item_size, gc.stats_size), pl = PL));

    left_lines.push(format!("{:<pl$}", "", pl = PL));
    let hdr = format!("  {:<5} {:>4}  {:>12}  {:>12}  {:>10}  {:>11}", "gen", "slot", "collections", "collected", "heap", "duration(s)");
    let hdr_len = hdr.len();
    left_lines.push(hdr);
    left_lines.push(format!("  {}", "-".repeat(hdr_len - 2)));

    for slot in &gc.slots {
        let gen_label = format!("{}", slot.generation);
        let heap = fmt_bytes(slot.heap_size as u64);
        left_lines.push(format!("  {:<5} {:>4}  {:>12}  {:>12}  {:>10}  {:>11.3}", gen_label, slot.slot, slot.collections, slot.collected, heap, slot.duration));
    }

    // right panel: hex dump of first slot
    let display_bytes = item_size.min(gc.raw_stats_bytes.len());
    // Drive the slot-items table from the version's actual per-slot layout rather than a
    // hardcoded 8-name list: the `+inc` build's slot has 26 fields (extra size/count/timing
    // fields appended after `heap_size`), and only `gc_layout.fields` reflects that. Same
    // `offset_table` computed above.
    let slot_fields: &[(&str, usize)] = offset_table.gc_layout.map(|l| l.fields).unwrap_or(&[]);
    right_lines.push(format!("{:<pr$}", format!("First slot of stats buffer (~{} bytes/slot):", item_size), pr = PR));
    for chunk in gc.raw_stats_bytes[..gc.raw_stats_bytes.len().min(display_bytes)].chunks(16) {
        let base = chunk.as_ptr() as usize - gc.raw_stats_bytes.as_ptr() as usize;
        let mut hex = String::new();
        let mut ascii = String::new();
        for (i, &b) in chunk.iter().enumerate() {
            if i == 8 { hex.push(' '); }
            write!(hex, "{:02x} ", b).unwrap();
            let ch = if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' };
            ascii.push(ch);
        }
        right_lines.push(format!("  {:08x}  {:<w$} |{}", base, hex.trim_end(), ascii, w = HEX_COL_WIDTH));
    }
    // slot field table (like GC State format) with borders
    let dashes = PR - 12;
    let tw = dashes - 2;
    right_lines.push(format!("  +{}+", "-".repeat(dashes)));
    right_lines.push(format!("  | {:<tw$} |",
        format!("GC Generation Stats Slot 1 @ {:#x}", gc.stats_addr), tw = tw));
    right_lines.push(format!("  +{}+", "-".repeat(dashes)));
    // Width the name column to the widest field this build actually has, so the `@ +offset`
    // and value columns stay aligned even for the long `+inc` names (e.g.
    // `ts_handle_weakref_callbacks_start`). Floored at 15 so short-field builds are unchanged.
    let name_w = slot_fields.iter().map(|(n, _)| n.len()).max().unwrap_or(0).max(15);
    let raw = &gc.raw_stats_bytes;
    for (name, offset) in slot_fields {
        let offset = *offset;
        if offset + 8 > raw.len() || offset >= display_bytes { continue; }
        let val = u64::from_le_bytes(raw[offset..offset + 8].try_into().unwrap());
        let fmt = if *name == "duration" {
            let d = f64::from_le_bytes(raw[offset..offset + 8].try_into().unwrap());
            format!("{:.6}", d)
        } else if name.starts_with("ts_") {
            fmt_thousands(val)
        } else if val > 0xFFFF_FFFF {
            format!("{:#x}", val)
        } else {
            format!("{}", val)
        };
        right_lines.push(format!("  | {:<tw$} |",
            format!("  {:<name_w$} @ +{:<4}  {}", name, offset, fmt, name_w = name_w), tw = tw));
    }
    right_lines.push(format!("  +{}+", "-".repeat(dashes)));

    let body = panels(&left_lines, &right_lines);

    writeln!(s, "{}", top()).unwrap();
    writeln!(s, "{}", l(&format!("GC Generation Stats Buffer @ {:#x}  (size: {} bytes)", gc.stats_addr, gc.stats_size))).unwrap();
    writeln!(s, "{}", sep()).unwrap();
    for line in &body {
        writeln!(s, "{}", line).unwrap();
    }
    writeln!(s, "{}", bot()).unwrap();
    writeln!(s).unwrap();
    s
}

// -- Hex dump panel (right side) -----------------------------
fn hex_panel(bytes: &[u8], limit: usize, base_off: usize, _panel_w: usize) -> Vec<String> {
    let display = bytes.len().min(limit);
    let data = &bytes[..display];
    let mut lines = Vec::new();
    for chunk in data.chunks(16) {
        let base = chunk.as_ptr() as usize - bytes.as_ptr() as usize + base_off;
        let mut hex = String::new();
        let mut ascii = String::new();
        for (i, &b) in chunk.iter().enumerate() {
            if i == 8 { hex.push(' '); }
            write!(hex, "{:02x} ", b).unwrap();
            let ch = if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' };
            ascii.push(ch);
        }
        // Pad the hex column to the full-row width so the ascii column stays aligned on a
        // short final row (region length not a multiple of 16).
        lines.push(format!("  {:08x}  {:<w$} |{}", base, hex.trim_end(), ascii, w = HEX_COL_WIDTH));
    }
    lines
}

/// Rendered width of the hex-bytes column for a full 16-byte row: 16 * "XX " (48) plus the
/// mid-gap space before byte 8 (49) minus the trailing space stripped by `trim_end` (48).
const HEX_COL_WIDTH: usize = 48;

// -- Format helpers -------------------------------------------
fn fmt_val(val: u64) -> String {
    if val > 0xFFFF_FFFF {
        format!("{:#x}", val)
    } else if val > 0x10000 {
        format!("{}", val)
    } else {
        format!("{}", val)
    }
}

fn fmt_thousands(val: u64) -> String {
    let s = val.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 { out.push('_'); }
        out.push(c);
    }
    out
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

fn fmt_duration(dur: f64) -> String {
    if dur < 1.0 {
        format!("{:.3}ms", dur * 1000.0)
    } else {
        format!("{:.3}s", dur)
    }
}

fn fmt_rate(rate: f64) -> String {
    if rate >= 10.0 {
        format!("{:.1}/s", rate)
    } else if rate >= 0.01 {
        format!("{:.2}/s", rate)
    } else {
        "0.0/s".to_string()
    }
}
