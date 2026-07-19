//! Shared `_Py_DebugOffsets` tree helpers for the ascii and TUI renderers.
//!
//! Originally this module also built an SVG diagram (`render_svg` + the
//! `diagram` subcommand); that image path was removed as obsolete. What remains
//! is the version-correct debug-offsets tree model consumed by `ascii.rs` and
//! `tui_v2.rs` to draw the `_Py_DebugOffsets` field subtree (3.13+ only).

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
