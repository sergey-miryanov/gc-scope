# 0007 — Cache the TUI's immutable per-PID content instead of rebuilding it every frame

- **Status:** Not started
- **Kind:** feature — efficiency
- **Effort:** M
- **Origin:** 2026-07-18 review (finding E4); the sibling finding E1 landed as
  [ADR 0001](../docs/adr/0001-pysession-resolve-once-facade.md).
- **Respects:** [ADR 0001](../docs/adr/0001-pysession-resolve-once-facade.md),
  [ADR 0009](../docs/adr/0009-performance-guarded-by-shape.md)

## 1. Problem statement

An operator who wants to watch GC activity closely runs the TUI at a fast refresh rate.
What they get back is a process burning CPU to re-render content that has not changed: the
`_Py_DebugOffsets` field tree, its guide prefixes, and the hex dump of the offsets block
are rebuilt from scratch every frame, allocating a fresh set of owned lines and spans each
time to produce byte-identical output. At the default rate this is invisible. At `--rate
10` it is the dominant cost in a loop whose actual work is a handful of remote reads —
measured at roughly 43 µs per frame when ADR 0001 landed, though on the since-removed
`ascii --watch` path, so treat the figure as an order of magnitude rather than a baseline.

The irony is that this is the *last* remaining instance of the problem ADR 0001 was written
to solve. Attach-time work is resolved once; render-time work is not.

## 2. Solution

The TUI does per-frame work proportional to what actually changed. The immutable panels are
built once when a PID is attached and reused until the operator attaches to a different
one; only the GC panel, whose numbers genuinely move, is rebuilt each tick. Nothing about
the display changes — same panels, same content, same keys. A fast `--rate` simply stops
costing what it costs today.

## 3. User stories

1. As a developer profiling my own script, I want the TUI at `--rate 10` to spend its CPU
   on reading the target rather than on re-allocating unchanged text, so that gcscope's own
   overhead does not perturb the process I am measuring.
2. As an operator attached to a production interpreter, I want gcscope's cost per frame to
   stay flat as I leave it running, so that a long observation session does not become a
   load source of its own.
3. As an operator, I want the `_Py_DebugOffsets` panel to look exactly as it does today, so
   that a performance change is invisible in the output I read.
4. As an operator, I want toggling the tree, the hex dump or the runtime hex to take effect
   immediately, so that caching never makes the UI feel stale.
5. As an operator switching targets with `p`, I want the new process's offsets panel to
   appear — not the previous one's — so that a cache can never show me another PID's data.
6. As a maintainer, I want `tui --output` to keep producing byte-identical dumps, so that
   the frame-dump tests remain a usable regression baseline.
7. As a maintainer, I want the uncached builder to remain the tested path, so that a cache
   bug cannot hide behind a test that exercises different code from what ships.

## 4. Implementation decisions

**What is cacheable.** `tui::layout::build_lines` assembles three groups: the
`_Py_DebugOffsets` section (the field tree from `tui::tree`, its prefixes, and the hex
dump), the interpreter section, and the GC-stats section. The first is a pure function of
the *resolved layout*, which `PySession` fixes at attach time and which cannot change for a
live PID; the offsets block itself is written once at interpreter startup. The GC section
is the live one.

**Where the cache lives.** In the frame loop's app state, next to the `PySession` — the
same lifecycle boundary that already exists, so invalidation is the PID change and nothing
else. It does **not** go inside `build_lines`: that function stays a pure, uncached builder
so `render_snapshot` and the existing layout tests keep exercising the code that ships.

**Keying.** On the render flags that select *which* static content is shown — the tree, hex
and runtime-hex toggles. Either cache per flag combination or rebuild on toggle; rebuilding
is fine, since a toggle is a human keypress and one rebuild at human speed is free. Prefer
whichever is simpler to reason about, and say which was chosen in the commit.

**Colour maps.** If the section builders still construct lookup maps per call, make them
`static` tables. That is a strictly local change and can land independently of the cache.

**Rejected — caching the snapshot.** `snapshot::collect` runs per frame by design; it is
the part that must stay live. Nothing about the collection path is in scope.

## 5. Seams and testing decisions

- **Seam:** `gcscope::tui::render_snapshot` behind `tui --output`, already used by
  `tests/tui.rs`. The highest seam available, and the right one: it renders a *whole frame
  as text*, so "the cache changed what the operator sees" is directly observable.
- **New seam needed:** none for correctness. For the efficiency claim itself, expose a
  counter or reuse the honest-signal pattern of `PySession::layout_source()` — a public,
  cheap "was this rebuilt or reused" signal — rather than a `test-hooks` gate. ADR 0005
  chose exactly that trade before, and the same reasoning applies: the tested configuration
  should be the shipped one.
- **What makes a good test here:** for correctness, byte-identical frame output across the
  three tier shapes; for efficiency, an **op-count invariant**, not a wall-clock
  measurement. [ADR 0009](../docs/adr/0009-performance-guarded-by-shape.md) is explicit
  that timing a loop dominated by syscalls measures the OS rather than our code — so assert
  "the static builder ran once per (PID, flags)", which is the actual claim.
- **Prior art:** `tests/tui.rs` for the live render path; `build_lines_on_a_legacy_snapshot_…`
  and the `render_snapshot` unit tests in `tui::layout` for the flattened-output assertions;
  `PySession::layout_source()` and the lifecycle tests in `tests/lifecycle.rs` as the model
  for observing a cache hit through an honest public signal.
- **Cases:**
  1. Frame dumps byte-identical before and after, on a 3.13 target (basic tier), a 3.15
     target (full tier) and a pre-3.13 target (legacy) — the three section shapes.
  2. The static builder runs once across N frames at a fixed PID and fixed flags.
  3. Toggling each flag produces the updated panel — cache invalidation on the flag axis.
  4. Switching PID with `p` produces the new target's panel — cache invalidation on the PID
     axis, and the case where a bug would be a *wrong-data* bug rather than a slow one.

## 6. Out of scope

- Caching anything across PIDs, or persisting to disk.
- The snapshot collection path, which stays per-frame (§4).
- Reworking how the TUI schedules frames. If the loop redraws when nothing changed, that is
  a separate question about the event loop, not about what a frame costs.

## 7. Further notes

Sequence this after [spec 0005](0005-tui-ring-geometry-from-layout.md): 0005 changes the
tree's geometry inputs, so caching the tree first would mean re-deriving the cache key
immediately afterwards. Landing 0005 first also produces the byte-identical baseline this
spec's case 1 needs.
