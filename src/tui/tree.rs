//! Shared `_Py_DebugOffsets` tree model for the TUI renderer.
//!
//! Originally this module also built an SVG diagram (`render_svg` + a `diagram`
//! subcommand) and fed an ASCII renderer; both were removed as obsolete. What
//! remains is the version-correct debug-offsets tree model consumed by `frame.rs`
//! to draw the `_Py_DebugOffsets` field subtree (3.13+ only).

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
    // The fixed prefix of the tree: offsets 0..88 are identical in every
    // `_Py_DebugOffsets` layout, so they are literals rather than table lookups.
    let mut e = vec![
        // depth 0
        TreeEntry { depth: 0, label: "_Py_DebugOffsets", kind: TreeEntryKind::Group },
        // depth 1
        TreeEntry { depth: 1, label: "cookie[8]",          kind: TreeEntryKind::RawValue { offset: 0 } },
        TreeEntry { depth: 1, label: "version",            kind: TreeEntryKind::RawValue { offset: 8 } },
        TreeEntry { depth: 1, label: "free_threaded",      kind: TreeEntryKind::RawValue { offset: 16 } },
        TreeEntry { depth: 1, label: "runtime_state",      kind: TreeEntryKind::Group },
        // depth 2 under runtime_state
        TreeEntry { depth: 2, label: "size",               kind: TreeEntryKind::RawValue { offset: 24 } },
        TreeEntry { depth: 2, label: "finalizing",         kind: TreeEntryKind::RawValue { offset: 32 } },
        TreeEntry { depth: 2, label: "interpreters_head",  kind: TreeEntryKind::RawValue { offset: 40 } },

        TreeEntry { depth: 1, label: "interpreter_state",  kind: TreeEntryKind::Group },
        // depth 2 under interpreter_state
        TreeEntry { depth: 2, label: "size",               kind: TreeEntryKind::RawValue { offset: 48 } },
        TreeEntry { depth: 2, label: "id",                 kind: TreeEntryKind::RawValue { offset: 56 } },
        TreeEntry { depth: 2, label: "next",               kind: TreeEntryKind::RawValue { offset: 64 } },
        TreeEntry { depth: 2, label: "threads_head",       kind: TreeEntryKind::RawValue { offset: 72 } },
        TreeEntry { depth: 2, label: "threads_main",       kind: TreeEntryKind::RawValue { offset: 80 } },
        TreeEntry { depth: 2, label: "gc",                 kind: TreeEntryKind::RawValue { offset: 88 } },
    ];

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
            // NOTE: both branches of the original `if has_sibling_after(..)` here pushed
            // "+-- ", so the last-child connector "\-- " promised by this function's doc
            // comment has never actually been emitted. Collapsed to match the behavior
            // that ships; emitting "\\-- " in the else branch is the presumable intent,
            // but it changes rendered output in the SVG, ASCII and TUI views.
            prefix.push_str("+-- ");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every layout shares the fixed 0..88 prefix, and the `gc` sub-struct's own
    /// fields are appended at depth 3. Without a `generation_stats` field (inline
    /// 3.13/3.14) there is no ring subtree.
    #[test]
    fn tree_has_the_fixed_prefix_and_no_ring_subtree_without_generation_stats() {
        let tree = debug_offsets_tree(&[("size", 96), ("collecting", 104)], None);
        assert_eq!(tree[0].label, "_Py_DebugOffsets");
        assert!(tree.iter().any(|e| e.label == "cookie[8]"));
        assert!(tree.iter().any(|e| e.label == "gc" && e.depth == 2));
        assert!(tree.iter().any(|e| e.label == "size" && e.depth == 3));
        assert!(tree.iter().any(|e| e.label == "collecting" && e.depth == 3));
        assert!(!tree.iter().any(|e| e.label == "young_slots (11)"));
    }

    /// A `generation_stats` field (ring builds) grows the derived per-generation
    /// slot subtree, and the resolved slot fields nest under it at depth 6.
    #[test]
    fn tree_adds_the_ring_subtree_when_generation_stats_is_present() {
        let gc_fields = [("size", 96u64), ("collecting", 104), ("generation_stats", 112)];
        let slot_fields = [("ts_start", 0usize), ("collections", 8)];
        let tree = debug_offsets_tree(&gc_fields, Some(&slot_fields));
        assert!(tree.iter().any(|e| e.label == "young_slots (11)" && e.depth == 4));
        assert!(tree.iter().any(|e| e.label == "old0_slots (3)"));
        assert!(tree.iter().any(|e| e.label == "ts_start" && e.depth == 6));
        assert!(tree.iter().any(|e| e.label == "collections" && e.depth == 6));
    }

    /// Characterization guard: `tree_prefixes` documents a "\-- " last-child
    /// connector it has never actually emitted — every connector is "+-- ". Lock the
    /// shipping behavior so changing it (the note calls it the presumable intent) is a
    /// deliberate, reviewed change that updates this test on purpose.
    #[test]
    fn tree_prefixes_always_use_the_plus_connector_never_the_last_child_form() {
        let tree = debug_offsets_tree(&[("size", 96), ("collecting", 104)], None);
        let prefixes = tree_prefixes(&tree);
        assert_eq!(prefixes[0], "", "the root entry has no connector");
        for (e, p) in tree.iter().zip(&prefixes).skip(1) {
            assert!(p.ends_with("+-- "), "depth {} prefix was {p:?}", e.depth);
            assert!(!p.contains("\\-- "), "last-child connector leaked into {p:?}");
        }
    }

    /// The derived slot geometry is `(region_size - 24) / 17` for the item size, with
    /// 11 young + 3 + 3 old slots and 8-byte index gaps. Below the 24-byte header there
    /// is no geometry (item size 0).
    #[test]
    fn gen_stats_layout_derives_slot_geometry_from_the_region_size() {
        // item_size 40 → 24 + 17*40 = 704.
        assert_eq!(gen_stats_layout(704), (40, 440, 120, 440, 568, 696, 448));
        assert_eq!(gen_stats_layout(0).0, 0);
        assert_eq!(gen_stats_layout(23).0, 0);
    }
}
