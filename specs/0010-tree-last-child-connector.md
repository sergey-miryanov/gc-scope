# 0010 — Emit the last-child connector `tree_prefixes` promises

**Status:** Not started, **pinned**
(`tree_prefixes_always_use_the_plus_connector_never_the_last_child_form` in `tui::tree`)
**Kind:** bug — cosmetic
**Effort:** S, but it changes rendered output
**Origin:** surfaced 2026-07-21 while adding coverage to the diagram subsystem; recorded
rather than fixed, and pinned by a characterization test so that fixing it is deliberate.
**Respects:** [ADR 0005](../docs/adr/0005-testing-strategy.md)

## 1. Problem

The `_Py_DebugOffsets` tree reads as though every branch continues. Each node — including
the last child of every subtree — is drawn with a `+-- ` connector, the form that implies a
sibling follows; the `\-- ` that closes a branch never appears. An operator scanning the
tree gets no visual signal for where a subtree ends, which is the one thing the connectors
exist to convey.

## 2. Evidence

`tui::tree::tree_prefixes` documents both connectors and emits one. The current code says
so outright:

```rust
if e.depth > 0 {
    // NOTE: both branches of the original `if has_sibling_after(..)` here pushed
    // "+-- ", so the last-child connector "\-- " promised by this function's doc
    // comment has never actually been emitted. …
    prefix.push_str("+-- ");
}
```

The `has_sibling_after` helper *is* consulted, but only for the vertical **ancestor
guides** (`|   ` versus four spaces) — never for the connector. So the guides are right and
the connectors are uniformly wrong, which is why the tree stays legible enough that nobody
filed it.

## 3. Scope

**Affected:** the `_Py_DebugOffsets` panel in interactive `tui` and in `tui --output`, on
3.13+ targets (pre-3.13 has no such block to draw).

**Not affected:** every decoded value. This is guide characters only.

**Pinned, not merely unfixed.** A characterization test locks the shipping behavior:

```rust
for (e, p) in tree.iter().zip(&prefixes).skip(1) {
    assert!(p.ends_with("+-- "), ...);
    assert!(!p.contains("\\-- "), "last-child connector leaked into {p:?}");
}
```

When the fix lands this test **will and should fail** — updating it is part of the change,
not collateral from it.

## 4. Proposed change

Branch the connector on whether the entry is the last child at its own depth:

```rust
if e.depth > 0 {
    if is_last_child(entries, i, e.depth) {
        prefix.push_str("\\-- ");
    } else {
        prefix.push_str("+-- ");
    }
}
```

**Do not reuse `has_sibling_after` verbatim.** It answers "is there *any* later entry at
this depth", which is not subtree-scoped: a later node at the same depth may belong to a
*following sibling subtree*, so a genuinely-last child is misreported as non-last. The
correct test scans forward and stops as soon as an entry at a **shallower** depth appears,
since that closes the current subtree:

```rust
fn is_last_child(entries: &[TreeEntry], i: usize, depth: u8) -> bool {
    for e in &entries[i + 1..] {
        if e.depth < depth { break; }          // left this subtree — no later sibling
        if e.depth == depth { return false; }  // a sibling follows
    }
    true
}
```

`has_sibling_after` has the identical latent scoping bug for the ancestor guides. Fixing it
too is defensible, but it moves the guide output as well as the connectors — decide
deliberately, and if both change, capture the baseline first.

## 5. Seams and testing decisions

- **Seam:** the pure `tree_prefixes` unit test in `tui::tree`, plus
  `gcscope::tui::render_snapshot` for the rendered result. Both exist; the unit test is
  where the connector logic is pinned, the snapshot is where "the operator actually sees
  it" is proved.
- **New seam needed:** none.
- **What makes a good test here:** assert connectors against a tree with **nested**
  subtrees. A flat tree passes with the naive `has_sibling_after` implementation, so a test
  built on one would ratify the very bug §4 warns about. The test must therefore encode the
  scoping rule, not just the character.
- **Prior art:** the existing tree tests in `tui::tree`
  (`tree_has_the_fixed_prefix_and_no_ring_subtree_without_generation_stats`,
  `tree_adds_the_ring_subtree_when_generation_stats_is_present`) build synthetic trees for
  exactly this kind of assertion; `tests/tui.rs` renders the real thing.
- **Cases:**
  1. Last child of a nested subtree gets `\-- `; a middle child gets `+-- `. The nesting is
     the point.
  2. A last child *followed by* a node at the same depth in a different subtree still gets
     `\-- ` — the case `has_sibling_after` gets wrong.
  3. The root entry keeps its empty prefix.
  4. Ancestor guides unchanged, unless `has_sibling_after` is fixed in the same change — in
     which case diff a full frame dump and review the movement deliberately.
  5. The characterization test is *replaced*, not deleted: it should now assert the new
     output with equal specificity.

## 6. Out of scope

Switching to Unicode box-drawing characters. The ASCII forms are deliberate — the frame
dump is a plain-text file consumed by tests and by operators redirecting output — and this
spec only makes the existing forms correct.

## 7. Further notes

Sequence relative to [spec 0008](0008-shared-formatters-and-pid-table.md): that one
requires byte-identical output, this one changes it. Land 0008 first, or keep them well
apart, so neither review has to disentangle intended output changes from unintended ones.
