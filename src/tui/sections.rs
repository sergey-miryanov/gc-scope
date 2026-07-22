//! The `_Py_DebugOffsets` half of the full layout (3.13+): section 1 renders the struct as a
//! version-correct field tree beside its hexdump; section 2 renders the focused
//! `PyInterpreterState` + GC-state box. Pre-3.13 has no such struct, so it gets the stripped
//! [`section_interpreter_legacy`] header instead. The GC generation-stats table (all tiers)
//! lives in `gc_view`.
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::remote_debugging::offsets::VersionedOffsets;
use crate::snapshot::collect::CollectedData;

use super::format::fmt_val;
use super::layout::{
    full_line, hex_dump_rows, l, padding_hex_right, plain_line, span_line, styled_left_inner_box,
    top, INNER_TW, INNER_W, PL,
};
use super::tree::{debug_offsets_tree, gen_stats_layout, tree_prefixes};

// ── Section 1: _Py_DebugOffsets ───────────────────────────────────
pub(super) fn section_debug_offsets(data: &CollectedData, off: &VersionedOffsets, show_tree: bool, show_hex: bool, show_runtime_hex: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let debug_size = data.debug_offsets_size as usize;

    lines.push(Line::from(Span::raw(top())));
    if !show_tree && !show_hex {
        lines.push(Line::from(Span::raw(l(&format!(
            "_Py_DebugOffsets (embedded in _PyRuntime) @ {:#x}  (size: {} bytes)  [hidden - press t/h to show]",
            data.runtime_addr, debug_size
        )))));
        lines.push(Line::from(Span::raw(top())));
        return lines;
    }
    lines.push(Line::from(Span::raw(l(&format!(
        "_Py_DebugOffsets (embedded in _PyRuntime) @ {:#x}  (size: {} bytes)",
        data.runtime_addr, debug_size
    )))));
    lines.push(Line::from(Span::raw(top())));

    let bytes = &data.runtime_raw_bytes;

    let (hex_start, hex_label) = if show_runtime_hex {
        (debug_size, "Runtime")
    } else {
        (0, "DebugOffsets")
    };
    let hex_range_end = hex_start + debug_size;
    let hex_slice = if hex_range_end <= bytes.len() {
        &bytes[hex_start..hex_range_end]
    } else if hex_start < bytes.len() {
        &bytes[hex_start..]
    } else {
        &[]
    };

    // `gc.generation_stats_size` is read from the target's `_Py_DebugOffsets`, so the
    // accessor already holds the process-published value (0 on builds without the field).
    let gen_stats_size = off.gc_generation_stats_size();
    let gs = gen_stats_layout(gen_stats_size);

    // Drive the GC-state subtree from actual, version-correct layout: the `gc`
    // sub-struct fields and the resolved per-entry field layout (which reflects the
    // clean-vs-`+inc` selection).
    let gc_fields = off.gc_debug_fields();
    let offset_table = off.to_offset_table(data.pid, data.runtime_addr);
    let entry_fields = offset_table.gc_layout.map(|l| l.fields);
    let tree = debug_offsets_tree(&gc_fields, entry_fields);
    let prefixes = tree_prefixes(&tree);

    let debug_highlights = if !show_runtime_hex {
        off.debug_offsets_highlight_regions()
    } else {
        vec![]
    };
    let hex_highlights: Vec<(usize, u8, Color)> = debug_highlights.iter()
        .filter(|(off, len, _, _)| hex_slice.len() >= off + *len as usize)
        .map(|(off, len, label, _)| {
            let color = match *label {
                "cookie[8]" => Color::Green,
                "interpreters_head" => Color::Cyan,
                "next" => Color::Yellow,
                "gc" => Color::Magenta,
                _ => Color::White,
            };
            (*off, *len, color)
        })
        .collect();

    let hex_rows = if show_hex {
        hex_dump_rows(hex_slice, hex_slice.len(), &hex_highlights, hex_start)
    } else {
        vec![]
    };

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

    let derived_val = |label: &str| -> String {
        let (item_size, young_bytes, _old_bytes, i0, i1, i2, _o0) = gs;
        match label {
            "item_size" => format!("{}", item_size),
            "young_entries (11)" => format!("11 x {} = {} bytes", item_size, young_bytes),
            "index0" => format!("+{}", i0),
            "old0_entries (3)" => format!("3 x {} bytes", item_size),
            "index1" => format!("+{}", i1),
            "old1_entries (3)" => format!("3 x {} bytes", item_size),
            "index2" => format!("+{}", i2),
            _ => String::new(),
        }
    };

    let format_tree_line = |prefix: &str, offset_str: &str, name: &str, value_str: &str| -> String {
        let before = format!("{}{}{}", prefix, offset_str, name);
        let pad = PL.saturating_sub(before.len() + value_str.len());
        format!("{}{}{}", before, " ".repeat(pad), value_str)
    };

    let mut tree_highlight_rows: Vec<(usize, Color)> = Vec::new();

    let mut left_owned: Vec<String> = Vec::new();
    if show_tree {
        left_owned.push(format!("{:<pl$}", "Fields:", pl = PL));
        for (i, entry) in tree.iter().enumerate() {
            let pfx = &prefixes[i];
            let line = match entry.kind {
                super::tree::TreeEntryKind::RawValue { offset } => {
                    let val = read_u64(offset);
                    let f = fmt_val(val, entry.label);
                    format_tree_line(pfx, &format!("0x{:04x}  ", offset), entry.label, &f)
                }
                super::tree::TreeEntryKind::Group => {
                    format_tree_line(pfx, "", entry.label, "")
                }
                super::tree::TreeEntryKind::Derived => {
                    let val_str = derived_val(entry.label);
                    format_tree_line(pfx, "", entry.label, &val_str)
                }
                super::tree::TreeEntryKind::Layout { field_type: _, field_offset } => {
                    let val_str = format!("+{}", field_offset);
                    format_tree_line(pfx, "", entry.label, &val_str)
                }
            };
            left_owned.push(line);
            if let Some(color) = debug_tree_row_color(entry.label, entry.kind) {
                tree_highlight_rows.push((left_owned.len() - 1, color));
            }
        }
    } else {
        left_owned.push(format!("{:<pl$}", "[Tree hidden - press t to show]", pl = PL));
    }

    if !show_tree && show_hex {
        // Full-width hex dump (left panel area)
        let hex_len = hex_slice.len();
        let hex_end_off = hex_start + hex_len.saturating_sub(1);
        lines.push(Line::from(Span::raw(l(&format!(
            "Hex Dump ({}, 0x{:04x}-0x{:04x}, {} bytes):",
            hex_label, hex_start, hex_end_off, hex_len
        )))));
        for row in &hex_rows {
            let hex_content: String = row.iter().map(|s| s.content.as_ref()).collect::<Vec<_>>().concat();
            lines.push(Line::from(Span::raw(l(&hex_content))));
        }
    } else {
        let hex_header: String = if show_hex {
            let hex_len = hex_slice.len();
            let hex_end_off = hex_start + hex_len.saturating_sub(1);
            format!("Hex Dump ({}, 0x{:04x}-0x{:04x}, {} bytes):",
                hex_label, hex_start, hex_end_off, hex_len)
        } else {
            "[Hex dump hidden - press h to show]".into()
        };

        let max_rows = left_owned.len().max(1 + hex_rows.len());
        for i in 0..max_rows {
            let lv = left_owned.get(i).map(|s| s.as_str()).unwrap_or("");
            if i == 0 {
                lines.push(plain_line(lv, &hex_header));
            } else {
                let ri = i - 1;
                let right = if ri < hex_rows.len() {
                    padding_hex_right(hex_rows[ri].clone())
                } else {
                    vec![Span::raw(" ".repeat(super::layout::PR))]
                };
                if let Some(&(_, color)) = tree_highlight_rows.iter().find(|(idx, _)| *idx == i) {
                    let left_span = Span::styled(
                        format!("{:<pl$}", lv, pl = PL),
                        Style::new().bg(color).fg(Color::Black),
                    );
                    lines.push(span_line(vec![left_span], right));
                } else {
                    lines.push(full_line(lv, right));
                }
            }
        }
    }

    lines.push(Line::from(Span::raw(top())));
    lines
}

// ── Section 2: PyInterpreterState ─────────────────────────────────
// Pre-3.13 focused interpreter header: no `_Py_DebugOffsets`, so no field table or
// GC-state box — just the interpreter address/id and a note; GC stats follow below.
pub(super) fn section_interpreter_legacy(data: &CollectedData) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let interp = &data.interpreter;
    lines.push(Line::from(Span::raw(top())));
    lines.push(Line::from(Span::raw(l(&format!(
        "PyInterpreterState @ {:#x}  (id: {})",
        interp.addr, interp.id
    )))));
    lines.push(Line::from(Span::raw(top())));
    lines.push(Line::from(Span::raw(l(
        "pre-3.13: no _Py_DebugOffsets struct — showing GC generation stats only",
    ))));
    lines.push(Line::from(Span::raw(top())));
    lines
}

pub(super) fn section_interpreter(data: &CollectedData, off: &VersionedOffsets) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let interp = &data.interpreter;
    // Show the whole GC state struct (raw_bytes is read to exactly gc.size bytes), so the
    // dump matches the "GC struct (N bytes)" header. A fixed cap truncated larger structs
    // like the +inc build's 232-byte state.
    let hex_end = interp.gc.raw_bytes.len();

    lines.push(Line::from(Span::raw(top())));
    lines.push(Line::from(Span::raw(l(&format!(
        "PyInterpreterState @ {:#x}  (struct: {} bytes)",
        interp.addr,
        off.interpreter_state_size()
    )))));
    lines.push(Line::from(Span::raw(top())));

    // ── Left panel content ──
    enum LeftItem {
        Plain(String),
        Styled(String, Color),
    }

    let mut left_items: Vec<LeftItem> = Vec::new();
    left_items.push(LeftItem::Plain(format!(
        "{:<pl$}",
        "Key offset values stored in _Py_DebugOffsets:",
        pl = PL
    )));
    for f in data.runtime_offset_fields() {
        if f.name.starts_with("runtime_state") || f.name.starts_with("gc") {
            continue;
        }
        let v = fmt_val(f.value);
        left_items.push(LeftItem::Plain(format!("    {:<30}  {:>18}", f.name, v)));
    }
    left_items.push(LeftItem::Plain(format!("{:<pl$}", "", pl = PL)));
    left_items.push(LeftItem::Plain(format!("  +{}+", "-".repeat(INNER_W))));
    left_items.push(LeftItem::Plain(format!(
        "  | {:<tw$} |",
        format!("GC State @ {:#x}", interp.addr + interp.gc_offset),
        tw = INNER_TW
    )));
    left_items.push(LeftItem::Plain(format!("  +{}+", "-".repeat(INNER_W))));

    // The descriptor `gc` sub-struct is append-only and shorter on older builds (2 fields on
    // 3.13/3.14, all 5 on 3.15+), so list only the fields this version actually publishes —
    // otherwise absent fields render as phantom `@ gc+0` / NULL rows. `gc_debug_fields()` is
    // the same version-correct source Section 1 uses.
    let present: Vec<&'static str> = off.gc_debug_fields().into_iter().map(|(n, _)| n).collect();

    let collecting_off = off.gc_collecting() as usize;
    let collecting_val = interp.gc.raw_bytes.get(collecting_off).copied().unwrap_or(0);
    if present.contains(&"size") {
        left_items.push(LeftItem::Plain(format!(
            "  | {:<tw$} |",
            format!("  {:<15} (store)    {}", "size", interp.gc_size),
            tw = INNER_TW
        )));
    }
    if present.contains(&"collecting") {
        left_items.push(LeftItem::Styled(
            format!(
                "  {:<15} @ gc+{:<4}  {}",
                "collecting", collecting_off, collecting_val
            ),
            Color::Yellow,
        ));
    }

    let frame_off = off.gc_frame() as usize;
    let frame_val = if frame_off + 8 <= interp.gc.raw_bytes.len() {
        u64::from_le_bytes(interp.gc.raw_bytes[frame_off..frame_off + 8].try_into().unwrap())
    } else {
        0
    };
    if present.contains(&"frame") {
        left_items.push(LeftItem::Styled(
            format!(
                "  {:<15} @ gc+{:<4}  {:#x}",
                "frame", frame_off, frame_val
            ),
            Color::Cyan,
        ));
    }

    if present.contains(&"generation_stats_size") {
        left_items.push(LeftItem::Plain(format!(
            "  | {:<tw$} |",
            format!(
                "  {:<15} (store)    {}",
                "gen_stats_size",
                off.gc_generation_stats_size()
            ),
            tw = INNER_TW
        )));
    }

    let gen_stats_off = off.gc_generation_stats() as usize;
    let gen_stats_ptr = if gen_stats_off + 8 <= interp.gc.raw_bytes.len() {
        u64::from_le_bytes(interp.gc.raw_bytes[gen_stats_off..gen_stats_off + 8].try_into().unwrap())
    } else {
        0
    };
    let ptr_str = if gen_stats_ptr != 0 {
        format!("{:#x}", gen_stats_ptr)
    } else {
        "NULL".into()
    };
    if present.contains(&"generation_stats") {
        left_items.push(LeftItem::Plain(format!(
            "  | {:<tw$} |",
            format!("  {:<15} @ gc+{:<4}  {}", "gen_stats", gen_stats_off, ptr_str),
            tw = INNER_TW
        )));
    }
    left_items.push(LeftItem::Plain(format!("  +{}+", "-".repeat(INNER_W))));

    // ── Right panel: hex dump ──
    let right_header = format!("{:<pr$}", format!("GC struct ({} bytes) hex dump:", interp.gc_size), pr = super::layout::PR);

    // GC struct highlights: collecting field (8 bytes) + frame field (8 bytes). Two separate
    // gates: the outer one is presence + bounds — it keeps absent fields on 3.13/3.14 (whose
    // offset accessors return 0, so `frame_val` would read the `size` bytes at offset 0) from
    // painting a bogus highlight. The inner one is value != 0 — a live signal the GC is
    // collecting right now, so a region only lights up while it's actually busy.
    let mut gc_highlights: Vec<(usize, u8, Color)> = Vec::new();
    // Kept as nested ifs (presence/bounds vs. live value) for readability, not collapsed.
    #[allow(clippy::collapsible_if)]
    if collecting_val != 0 {
        if present.contains(&"collecting") && collecting_off + 8 <= interp.gc.raw_bytes.len() {
            gc_highlights.push((collecting_off, 8, Color::Yellow));
        }
    }
    #[allow(clippy::collapsible_if)]
    if frame_val != 0 {
        if present.contains(&"frame") && frame_off + 8 <= interp.gc.raw_bytes.len() {
            gc_highlights.push((frame_off, 8, Color::Cyan));
        }
    }

    let hex_rows = hex_dump_rows(&interp.gc.raw_bytes, hex_end, &gc_highlights, 0);

    // ── Combine ──
    let max_rows = left_items.len().max(1 + hex_rows.len());
    for i in 0..max_rows {
        let ri = i.saturating_sub(1);
        let right = if i == 0 {
            // Header row: left header + right header
            let lv = match left_items.first() {
                Some(LeftItem::Plain(s)) => s.as_str(),
                _ => "",
            };
            lines.push(plain_line(lv, &right_header));
            continue;
        } else if ri < hex_rows.len() {
            padding_hex_right(hex_rows[ri].clone())
        } else {
            vec![Span::raw(" ".repeat(super::layout::PR))]
        };

        match left_items.get(i) {
            Some(LeftItem::Plain(s)) => lines.push(full_line(s, right)),
            Some(LeftItem::Styled(s, c)) => {
                let left_spans = styled_left_inner_box(s, Some(*c));
                lines.push(span_line(left_spans, right));
            }
            None => lines.push(full_line("", right)),
        }
    }

    lines.push(Line::from(Span::raw(top())));
    lines
}

/// Highlight color for one `_Py_DebugOffsets` tree row, or `None` to leave it unshaded. The
/// named runtime fields match their hexdump-region colors (see the `hex_highlights` mapping);
/// every first-entry field item (`Layout` kind — the per-entry `gc_generation_stats` fields on
/// ring builds) shares one color so the entry's layout reads as a single group. That group is
/// tree-only: those items have no hexdump region of their own.
fn debug_tree_row_color(label: &str, kind: super::tree::TreeEntryKind) -> Option<Color> {
    use super::tree::TreeEntryKind;
    match label {
        "cookie[8]" => Some(Color::Green),
        "interpreters_head" => Some(Color::Cyan),
        "next" => Some(Color::Yellow),
        "gc" => Some(Color::Magenta),
        _ => matches!(kind, TreeEntryKind::Layout { .. }).then_some(Color::Blue),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    use crate::tui::test_support::{join_lines, legacy_data};

    #[test]
    fn section_interpreter_legacy_names_the_interpreter_and_flags_the_missing_struct() {
        let data = legacy_data(true);
        let out = join_lines(&section_interpreter_legacy(&data));
        assert!(out.contains("PyInterpreterState @ 0x6000  (id: 0)"), "{out}");
        assert!(out.contains("pre-3.13: no _Py_DebugOffsets"), "{out}");
    }

    #[test]
    fn debug_tree_row_color_shades_named_fields_and_every_first_entry_item() {
        use crate::tui::tree::TreeEntryKind;
        // Named runtime fields keep their hexdump-region colors.
        assert_eq!(debug_tree_row_color("cookie[8]", TreeEntryKind::RawValue { offset: 0 }), Some(Color::Green));
        assert_eq!(debug_tree_row_color("gc", TreeEntryKind::RawValue { offset: 88 }), Some(Color::Magenta));
        // Every first-entry field item (Layout) shares one color, whatever its field name.
        let layout = |off| TreeEntryKind::Layout { field_type: "", field_offset: off };
        assert_eq!(debug_tree_row_color("ts_start", layout(0)), Some(Color::Blue));
        assert_eq!(debug_tree_row_color("increment_size", layout(32)), Some(Color::Blue));
        // Groups, derived rows, and unnamed raw values stay unshaded.
        assert_eq!(debug_tree_row_color("young_entries (11)", TreeEntryKind::Derived), None);
        assert_eq!(debug_tree_row_color("version", TreeEntryKind::RawValue { offset: 8 }), None);
    }
}
