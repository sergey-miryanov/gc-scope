# CPython 3.14.5 GC hook-point inventory

Where an injected agent can interpose on the garbage collector of a running or embedded
CPython **3.14.5**, in order to reconstruct — inside the target — the per-collection
statistics that CPython **3.15** publishes natively in its GC stats ring buffer.

Everything below was read on **2026-08-05** out of the CPython source trees on disk:
the primary target is `X:\Work\cpython\3.14.5`, a clean checkout of tag **`v3.14.5`**
(`5607950ef232dad16d75c0cf53101d9649d89115`, `git status --porcelain` empty). The 3.15
comparison is `X:\Work\cpython\3.15` at **`v3.15.0b1-33-g6a660056998`**; two other trees
were consulted to date the GC changes, `X:\Work\cpython\3.14.0` (`v3.14.0`) and
`X:\Work\cpython\3.14.4` (`v3.14.4`). Windows symbol facts were not inferred from the
source — they were measured against the installed binary
`C:\Python\Python314.5\python314.dll`, which self-identifies as
`Python 3.14.5 (tags/v3.14.5:5607950, May 10 2026) [MSC v.1944 64 bit (AMD64)]`, i.e. the
same commit as the tree. Sibling-project context read: `gcscope/CLAUDE.md`,
`gcscope/CONTEXT.md`, `gcscope/docs/research/gcmon-inventory.md`.

> **On citations.** Same rule as [`gcmon-inventory.md`](gcmon-inventory.md): gcscope's own
> `specs/README.md` says to anchor on symbols rather than line numbers, and that rule is
> about *this* repo's code. CPython is an external source frozen at a tag here, so
> `file:line` ranges are cheaper and more precise. Every range below is pinned to
> **`v3.14.5` / `5607950`** unless the path is prefixed `3.15/`, in which case it is pinned
> to **`6a660056998`**. Re-check them against any other checkout. Docs, PEPs and PRs get
> full URLs. Export-table claims are from the measured DLL, not from the headers.

---

## Summary

**Three parts of the question's premise are wrong, and the third one changes the answer.**

1. **There is no `Modules/gc.c`.** The collector lives in `Python/gc.c` (2,057 lines,
   GIL builds) and `Python/gc_free_threading.c` (3,006 lines, free-threaded builds), with
   `Python/gc_gil.c` (17 lines) holding one free-list helper. `Modules/gcmodule.c` (581
   lines) is only the Python-level `gc` module wrapper. The two collector files are
   mutually exclusive at compile time: `Python/gc.c:20` opens
   `#if !defined(Py_GIL_DISABLED)` and closes at `:2057`;
   `Python/gc_free_threading.c:58` opens `#ifdef Py_GIL_DISABLED` and closes at `:3006`.

2. **3.14.5 is not an incremental collector.** It is the classic three-generation
   collector. `gc_collect_increment`, `gc_collect_full`, `gc_collect_young` and
   `gc_collect_region` — all of which *do* exist in `3.14.4/Python/gc.c:1280,1325,1614,1674`
   — are **absent from 3.14.5**. The incremental GC shipped in 3.14.0 through 3.14.4 and
   was replaced in 3.14.5 by a forward-port of the generational collector, in-tree at
   commits `9a7e205e463` ("[3.14] GH-148726: Forward-port generational GC (#148720)") and
   `4d0ae4cba98` ("[3.14] GH-148726: Add heap_size to `_gc_runtime_state` (#149025)"), both
   ancestors of the `v3.14.5` tag. Upstream PR:
   <https://github.com/python/cpython/pull/148720> (merged into `python:3.14` 2026-04-26).
   **Any hook plan keyed on 3.14 function names must say *which* 3.14 patch release.**

3. **3.14.5 does have something the question assumes only 3.15 has**: a live
   `heap_size` counter on the GC runtime state
   (`Include/internal/pycore_interp_structs.h:230-231`), maintained inline by
   `_PyObject_GC_TRACK`/`_PyObject_GC_UNTRACK`
   (`Include/internal/pycore_gc.h:233`, `:269`) — GIL builds only.

**The verdict on hooking.** Of the ~20 functions in the 3.14.5 GC, **exactly 15 are
exported from `python314.dll`, and not one of them is on the collection path.** Every
function that knows a collection is happening — `gc_collect_main`, `invoke_gc_callback`,
`gc_select_generation`, `gc_alloc` — is `static`, i.e. file-local, and therefore
uninterposable by symbol on any platform. The nearest external-linkage functions
(`_PyGC_Collect`, `_Py_RunGC`, `_PyObject_GC_Link`, `_Py_ScheduleGC`, `_PyGC_Freeze`) are
internal-API `extern` declarations with **no `PyAPI_FUNC`**, which means: not in the
Windows export table (measured), and hidden on ELF because `configure.ac:2596-2610` builds
the core with `-fvisibility=hidden`. Symbol interposition — `LD_PRELOAD`,
`dlsym(RTLD_NEXT)`, IAT patching, export-table patching — **cannot reach the 3.14.5
collector on either platform.** What remains is inline detouring (byte-patching a
function prologue at an address you found some other way), and that is a poor trade here
because the *supported* alternative gives you almost the same data.

**Recommendation: do not detour. Ship a C extension module that registers a
`gc.callbacks` entry and writes a 3.15-shaped ring into its own module-global buffer.**
It is documented, version-stable, ~200 lines, needs no address discovery, and reconstructs
**7 of the 8** fields of the 3.15 ring record. The one field it cannot supply is
`candidates`, because 3.14.5's `deduce_unreachable` is `static inline void`
(`Python/gc.c:1112-1113`) and never counts what 3.15's returns
(`3.15/Python/gc.c:396-397`, `Py_ssize_t update_refs(...)`). If `candidates` is
load-bearing, the only honest route is rebuilding the interpreter with the 3.15 ring
backported — which is 3 struct definitions and ~50 lines of `Python/gc.c`, and which the
author of this note already wrote once upstream
(<https://github.com/python/cpython/pull/146532>).

### Approach comparison

| # | Approach | Robustness | Windows feasibility | Data fidelity vs 3.15 ring | Invasiveness |
|---|---|---|---|---|---|
| 1 | **C extension + `gc.callbacks` + own ring** | **High** — public documented API, stable since 3.3 | **High** — no address discovery, no code patching, ships as a `.pyd` | **7/8 fields.** Misses `candidates`. `heap_size` needs one hardcoded offset or `_testinternalcapi` | Low — one `import`, or `PYTHONSTARTUP`/`sitecustomize` |
| 2 | **Backport the 3.15 ring to a rebuilt 3.14.5** | **High** — it is just CPython | Medium — requires building CPython on Windows and shipping that interpreter | **8/8, byte-identical**, and readable by gcscope's existing ring path | High — you now own an interpreter build |
| 3 | Pure-Python `gc.callbacks` (no C) | High | High | 5/8, and no out-of-process ring — data lives in Python objects | Lowest |
| 4 | **Inline detour of `_PyGC_Collect`** (byte patch) | Low — address must be found by pattern/PDB; catches only `gc.collect()`, **not** automatic collections | Medium — MinHook/Detours work, but the address is not in the export table | 7/8 in principle, 0/8 in practice for automatic GC | High |
| 5 | **Inline detour of `_Py_RunGC` / `gc_collect_main`** | Very low — `gc_collect_main` is `static`, may be inlined into its 4 callers; no symbol at all in a release build | Low — needs PDB symbols or a byte-pattern scan that breaks per patch release | 8/8 if it lands | Very high |
| 6 | IAT hooking of `_PyObject_GC_New` / `PyObject_GC_Del` | Low | Medium — works, but only catches **extension-module** callers (measured), never `python314.dll`'s own calls | Allocation pressure only; nothing about collections | High |
| 7 | `sys.monitoring` / audit hooks | n/a | n/a | **Zero — no GC events exist.** The only `gc.*` audit events are introspection calls | n/a |
| 8 | DTrace/USDT probes (`PyDTrace_GC_START/DONE`) | n/a on Windows | **None** — `Include/pydtrace.h` compiles the `*_ENABLED()` predicates to `return 0` when `WITH_DTRACE` is unset, and it is unset on Windows | n/a | n/a |

Approach 1 was subsequently **measured and built**. Runtime cost: +658 ns per collection
worst case, ~0.5% of a realistic young-gen collection and below that measurement's own
noise floor ([§11](#11-measured-what-a-gccallbacks-entry-actually-costs)). A working
prototype was then read out of a live 3.14.5 process by gcscope's own decoder with no
changes to the decode path, and located from a pid alone via the injected module's PE
export table ([§12](#12-verified-gcscope-reads-an-injected-3145-ring-unmodified)). Neither
changes the ranking; both decode and discovery are now demonstrated end to end.

---

## 1. The real file layout in 3.14.5

| File | Lines | Compiled when | What is in it |
|---|---|---|---|
| `Python/gc.c` | 2,057 | `!Py_GIL_DISABLED` (`:20`, `:2057`) | The whole GIL-build collector |
| `Python/gc_free_threading.c` | 3,006 | `Py_GIL_DISABLED` (`:58`, `:3006`) | The whole free-threaded collector — a separate implementation, not a variant |
| `Python/gc_gil.c` | 17 | `!Py_GIL_DISABLED` | `_PyGC_ClearAllFreeLists` only |
| `Modules/gcmodule.c` | 581 | always | The `gc` module: `gc.collect`, `gc.get_stats`, `gc.freeze`, and the module-level `gc.callbacks`/`gc.garbage` objects |
| `Modules/gc_weakref.txt` | — | — | Design notes, not code |

`Modules/gc.c` does not exist and, in this lineage, never did — the module wrapper has been
`Modules/gcmodule.c` throughout, and the collector moved to `Python/gc.c` before 3.13.

The GC runtime state itself is **not** in `pycore_gc.h`; it is
`struct _gc_runtime_state` in `Include/internal/pycore_interp_structs.h:202-266`, reached
as `interp->gc` (`Python/gc.c:104-109`).

---

## 2. What we are trying to reproduce: the 3.15 ring

3.15 replaced the flat `generation_stats[NUM_GENERATIONS]` array with a heap-allocated
`struct gc_stats` holding one ring per generation
(`3.15/Include/internal/pycore_interp_structs.h:180-222`):

```c
/* Running stats per generation */
struct gc_generation_stats {
    PyTime_t ts_start;
    PyTime_t ts_stop;
    /* total number of collections */
    Py_ssize_t collections;
    /* total number of collected objects */
    Py_ssize_t collected;
    /* total number of uncollectable objects (put into gc.garbage) */
    Py_ssize_t uncollectable;
    // Total number of objects considered for collection and traversed:
    Py_ssize_t candidates;
    // Total duration of the collection in seconds:
    double duration;
    /* heap_size on the start of the collection */
    Py_ssize_t heap_size;
};
```

Ring depth is `GC_YOUNG_STATS_SIZE = 11` and `GC_OLD_STATS_SIZE = 3` on GIL builds,
`1`/`1` on free-threaded (`3.15/…/pycore_interp_structs.h:198-213`), and the state holds a
`struct gc_stats *generation_stats` pointer rather than an inline array (`:237`).

The writer is `add_stats` (`3.15/Python/gc.c:1399-1418`). Two properties of it matter for
anything that tries to imitate it:

- **Records are cumulative, not per-collection.** `add_stats` `memcpy`s the previous ring
  entry over the new one, then adds this collection's deltas
  (`:1405-1413`). `collections`, `collected`, `uncollectable`, `candidates` and `duration`
  are lifetime totals; `ts_start`, `ts_stop` and `heap_size` are per-collection. This is
  exactly what gcmon's loss reconstruction depends on
  ([`gcmon-inventory.md` §2.4](gcmon-inventory.md)).
- **`ts_stop` is published last, deliberately**, so a remote reader never selects a torn
  record as the newest (`:1415-1417`). Any 3.14 imitation must keep that store order.

Field origins inside `gc_collect_main` (`3.15/Python/gc.c`): `heap_size` snapshot and
`ts_start` at `:1474-1476`, `candidates` from `deduce_unreachable`'s return at `:1509`,
`collected` accumulated at `:1555` and `:1580`, `uncollectable` at `:1585-1591`, `ts_stop`
and `duration` at `:1592-1593`, and `add_stats` at `:1624`.

3.15 also widened `gc.callbacks`: its `invoke_gc_callback` takes the whole
`struct gc_generation_stats *` and puts `candidates` and `duration` into the info dict
(`3.15/Python/gc.c:1257-1278`). 3.14.5's does not — see §5.

---

## 3. Hook-point inventory (GIL build, `Python/gc.c`)

Linkage column: **static** = file-local, no symbol to interpose. **internal** = external
C linkage but declared without `PyAPI_FUNC`, so hidden on ELF and absent from the Windows
export table. **public** = `PyAPI_FUNC`, present in `python314.dll` (measured).

| Symbol | Definition | Signature (as written) | Linkage | In `python314.dll`? | On the auto-collection path? |
|---|---|---|---|---|---|
| `gc_collect_main` | `Python/gc.c:1312-1533` | `static Py_ssize_t gc_collect_main(PyThreadState *tstate, int generation, _PyGC_Reason reason)` | **static** | no | **yes — the only place** |
| `invoke_gc_callback` | `:1200-1251` | `static void invoke_gc_callback(PyThreadState *tstate, const char *phase, int generation, Py_ssize_t collected, Py_ssize_t uncollectable)` | **static** | no | yes (twice per collection) |
| `gc_select_generation` | `:1257-1307` | `static int gc_select_generation(GCState *gcstate)` | **static** | no | yes, for `GENERATION_AUTO` only |
| `gc_alloc` | `:1886-1903` | `static PyObject *gc_alloc(PyTypeObject *tp, size_t basicsize, size_t presize)` | **static** | no | allocation path |
| `deduce_unreachable` | `:1112-1113` | `static inline void deduce_unreachable(PyGC_Head *base, PyGC_Head *unreachable)` | **static inline** | no | yes — and note `void` |
| `_PyGC_Collect` | `:1688-1692` | `Py_ssize_t _PyGC_Collect(PyThreadState *tstate, int generation, _PyGC_Reason reason)` | internal (`pycore_gc.h:318`) | **no** | **no** — only `gc.collect()` |
| `_PyGC_CollectNoFail` | `:1694-1704` | `void _PyGC_CollectNoFail(PyThreadState *tstate)` | internal (`pycore_gc.h:319`) | **no** | shutdown only |
| `_Py_RunGC` | `:1876-1884` | `void _Py_RunGC(PyThreadState *tstate)` | internal (`pycore_gc.h:333`) | **no** | **yes — the automatic entry point** |
| `_Py_ScheduleGC` | `:1845-1852` | `void _Py_ScheduleGC(PyThreadState *tstate)` | internal (`pycore_ceval.h`) | **no** | yes — sets the eval-breaker bit |
| `_PyObject_GC_Link` | `:1854-1874` | `void _PyObject_GC_Link(PyObject *op)` | internal (`pycore_object.h:865`, plain `void`, no macro) | **no** | allocation path; owns the threshold test |
| `_PyGC_Freeze` / `_PyGC_Unfreeze` / `_PyGC_GetFreezeCount` | `:1619-1641` | `void _PyGC_Freeze(PyInterpreterState *interp)` etc. | internal (`pycore_gc.h:321-326`) | **no** | no |
| `PyGC_Collect` | `:1671-1686` | `PyAPI_FUNC(Py_ssize_t) PyGC_Collect(void)` (`objimpl.h:150`) | **public** | **yes** | **no** — C-API manual collection only |
| `PyGC_Enable` / `PyGC_Disable` / `PyGC_IsEnabled` | `:1645-1667` | `PyAPI_FUNC(int) PyGC_Enable(void)` etc. (`objimpl.h:152-154`) | **public** | **yes** | no |
| `_PyObject_GC_New` | `:1906-1923` | `PyAPI_FUNC(PyObject *) _PyObject_GC_New(PyTypeObject *)` (`objimpl.h:165`) | **public** | **yes** | allocation path (partial — see §6) |
| `_PyObject_GC_NewVar` | `:1925-1942` | `PyAPI_FUNC(PyVarObject *) _PyObject_GC_NewVar(PyTypeObject *, Py_ssize_t)` (`objimpl.h:166`) | **public** | **yes** | allocation path (partial) |
| `_PyObject_GC_Resize` | `:1958-1975` | `PyAPI_FUNC(PyVarObject *) _PyObject_GC_Resize(PyVarObject *, Py_ssize_t)` (`objimpl.h:159`) | **public** | **yes** | no GC accounting at all |
| `PyUnstable_Object_GC_NewWithExtraData` | `:1944-1956` | `PyAPI_FUNC(PyObject *) PyUnstable_Object_GC_NewWithExtraData(PyTypeObject *, size_t)` (`cpython/objimpl.h:86-87`) | **public** | **yes** | rare allocation path |
| `PyObject_GC_Del` | `:1977-2004` | `PyAPI_FUNC(void) PyObject_GC_Del(void *)` (`objimpl.h:178`) | **public** | **yes** | deallocation path |
| `PyObject_GC_Track` / `PyObject_GC_UnTrack` | `:1809-1837` | `PyAPI_FUNC(void) PyObject_GC_Track(void *)` (`objimpl.h:171,176`) | **public** | **yes** | **mostly bypassed** — see §6 |
| `PyObject_GC_IsTracked` / `IsFinalized` / `PyObject_IS_GC` | `:2006-2022`, `:1839-1843` | `PyAPI_FUNC(int) …` (`objimpl.h:185-186`, `cpython/objimpl.h:78`) | **public** | **yes** | no |
| `PyUnstable_GC_VisitObjects` | `:2041-2055` | `PyAPI_FUNC(void) PyUnstable_GC_VisitObjects(gcvisitobjects_t callback, void* arg)` (`cpython/objimpl.h:104`) | **public** | **yes** | **never called by the GC** — it is a caller-driven walker |
| `_PyGC_Init` / `_PyGC_InitState` / `_PyGC_Fini` | `:112-148`, `:1756-1782` | `void _PyGC_InitState(GCState *)` etc. (`pycore_gc.h:316`) | internal | **no** | startup/shutdown |

**Measured export table.** `python314.dll` exports 1,811 symbols. Filtering for GC-related
names yields exactly fifteen:

```
PyGC_Collect          PyGC_Disable          PyGC_Enable           PyGC_IsEnabled
PyObject_GC_Del       PyObject_GC_IsFinalized  PyObject_GC_IsTracked
PyObject_GC_Track     PyObject_GC_UnTrack   PyObject_IS_GC
PyUnstable_GC_VisitObjects   PyUnstable_Object_GC_NewWithExtraData
_PyObject_GC_New      _PyObject_GC_NewVar   _PyObject_GC_Resize
```

(`_PyLong_GCD` and `Py_GetArgcArgv` also match the substring filter and are unrelated.)
That set matches `PC/python3dll.c:32-34,295-298,493-497` exactly — the stable-ABI
forwarder re-exports the same fifteen minus the three `PyUnstable_*`/`IS_GC` entries. **No
`_PyGC_*`, no `_Py_RunGC`, no `_Py_ScheduleGC`, no `_PyObject_GC_Link`.**

### Free-threaded build (`Python/gc_free_threading.c`)

Same public surface, different internals. Where it diverges materially:

| Symbol | Definition | Difference that matters |
|---|---|---|
| `gc_collect_main` | `:2310-2419` | Same shape as GIL, but the body is one call to `gc_collect_internal` (`:2186-2305`), which stops the world twice (`_PyEval_StopTheWorld` at `:2189` and `:2267`). |
| `invoke_gc_callback` | `:1874-1878` | Identical signature to the GIL build's — `(tstate, phase, generation, collected, uncollectable)`. Same five values, same two call sites (`:2343-2345`, `:2412-2414`). |
| `_PyObject_GC_Link` | `:2803-2807` | **Body is one line**: `record_allocation(_PyThreadState_GET());`. All the threshold logic moved to `record_allocation` (`:2139-2159`, `static`) and `gc_should_collect` (`:2117-2137`, `static`). Hooking `_PyObject_GC_Link` here buys you nothing you did not already have. |
| `_Py_RunGC` | `:2809-2816` | Calls `gc_collect_main(tstate, 0, _Py_GC_REASON_HEAP)` — **always generation 0**, unlike the GIL build's `GENERATION_AUTO`. |
| `gc_alloc` | `:2818-2837` | Calls `record_allocation` directly, **not** `_PyObject_GC_Link`. |
| `PyObject_GC_Del` | `:2910-2932` | Calls `record_deallocation` (`:2161-2184`, `static`); does not touch `heap_size`. |
| `_PyGC_VisitObjectsWorldStopped` | `:2970-2979` | Extra internal entry point that does not exist in the GIL build (`pycore_gc.h:341-344`). |
| `heap_size` | — | **Never maintained.** `_PyObject_GC_TRACK`'s increment is inside `#else /* !Py_GIL_DISABLED */` (`pycore_gc.h:217-234`). On a free-threaded 3.14.5 the field stays 0. |

I did **not** measure a free-threaded `python314t.dll` — none is installed on this machine
(`py -0p` lists `3.13t` but no `3.14t`). The export reasoning carries over unchanged
because it follows from the `PyAPI_FUNC` macro in `Include/exports.h:56-88`, which is
build-flavour-independent, but the *measurement* is GIL-build only. Flagged.

---

## 4. Interposition mechanics on the platforms that matter

### Windows (the environment in question)

`Include/exports.h:27-35` defines `Py_EXPORTED_SYMBOL` as `__declspec(dllexport)` under
`_WIN32 && Py_ENABLE_SHARED`, and `:56-88` wires `PyAPI_FUNC` to it when `Py_BUILD_CORE`.
There is **no `.def` file** for `pythoncore` — `PCbuild/pythoncore.vcxproj` has no
`ModuleDefinitionFile`, and the only `.def` under `PC/` is `PC/pyshellext.def`. So the
export table is exactly the set of `PyAPI_FUNC`/`PyAPI_DATA` declarations, which is what
the measurement confirms.

Three mechanisms, in increasing order of what they can reach:

1. **Export-table patching** — only reaches the fifteen public symbols above, none of which
   is on the collection path. Useless for this problem.
2. **IAT hooking** — patches one *importing module's* import table. Measured: `_ctypes.pyd`
   and `_asyncio.pyd` both carry `_PyObject_GC_New`, `PyObject_GC_Del` and
   `PyObject_GC_UnTrack` as named imports; `_socket.pyd` carries only
   `PyObject_GC_UnTrack`. So IAT hooking works — **for extension modules only**. MSVC
   resolves `python314.dll`'s own calls to its own exported functions as direct relative
   calls at link time; there is no IAT entry for a symbol the module exports itself.
   `Objects/listobject.c:244`'s `PyObject_GC_New(PyListObject, &PyList_Type)` — a macro
   expanding to `_PyObject_GC_New` (`Include/objimpl.h:180-183`) — is a direct intra-DLL
   call and is invisible to any IAT hook. **IAT hooking gives you a biased sample of
   allocations and nothing about collections.**
3. **Inline detour** (MinHook, Detours, hand-rolled trampoline) — the only mechanism that
   can reach `_PyGC_Collect`, `_Py_RunGC` or `gc_collect_main`. Costs: you must first
   *find* the address, and none of the three is in the export table. That means either the
   matching PDB (Microsoft publishes symbols for released `python314.dll`, so this is
   viable but is a per-patch-release dependency) or a byte-pattern scan (fails silently
   across patch releases — the failure mode `gcscope`'s
   [ADR 0004](../adr/0004-per-platform-image-layout.md) warns about: it fails **open**).
   For `gc_collect_main` specifically there may be **no distinct function at all** — it is
   `static` with four call sites (`Python/gc.c:1682,1703,1883` and `_PyGC_Collect` at
   `:1691`), and MSVC is free to inline or ICF-fold it.

### ELF

`configure.ac:2596-2610` adds `-fvisibility=hidden` to `CFLAGS_NODIST` whenever the
compiler supports it. Combined with `Include/exports.h:44-48` (`PyAPI_FUNC` →
`__attribute__((visibility("default")))`), the result is the same partition as Windows:
the fifteen public GC symbols are in `.dynsym`, the internal ones are not.
`LD_PRELOAD` + `dlsym(RTLD_NEXT)` therefore **cannot** interpose `_PyGC_Collect` or
`_Py_RunGC`. On a default (non-`--enable-shared`) Linux build the collector is statically
linked into the `python` executable, which removes even the shared-library interposition
story. Windows and ELF agree here; there is no platform where the symbol route works.

---

## 5. Call chains — what is actually reached

### Automatic collection (the one that matters)

```
_PyObject_GC_Link            Python/gc.c:1854-1874        (internal, not exported)
  └─ generations[0].count++ ; if count > threshold && enabled && !collecting
       └─ _Py_ScheduleGC    Python/gc.c:1845-1852         (internal, not exported)
            └─ sets _PY_GC_SCHEDULED_BIT on the eval breaker
                 │
                 ├─ Python/ceval_gil.c:1396-1400   (eval loop, the normal path)
                 │     └─ _Py_RunGC(tstate)
                 └─ Modules/signalmodule.c:1781-1785  (PyErr_CheckSignals, for long-running C code)
                       └─ _Py_RunGC(tstate)
                            └─ if (!gcstate->enabled) return;
                                 └─ gc_collect_main(tstate, GENERATION_AUTO, _Py_GC_REASON_HEAP)
                                                          Python/gc.c:1312   (static)
```

`_Py_RunGC` has exactly two callers, both verified by grep across `Python/`, `Modules/`
and `Include/`. The `signalmodule.c` one is easy to miss and is not on the eval loop; a
hook that only watches the eval breaker will miss collections triggered from long-running
C extension code.

### Manual collection

```
gc.collect(gen)  → Modules/gcmodule.c:83-95  gc_collect_impl
                     └─ _PyGC_Collect(tstate, generation, _Py_GC_REASON_MANUAL)   :94
PyGC_Collect()   → Python/gc.c:1671-1686
                     └─ gc_collect_main(tstate, NUM_GENERATIONS - 1, _Py_GC_REASON_MANUAL)  :1682
shutdown         → _PyGC_CollectNoFail → gc_collect_main(..., _Py_GC_REASON_SHUTDOWN)  :1703
```

Note the asymmetry: `PyGC_Collect` (public, exported) does **not** go through
`_PyGC_Collect`; it calls `gc_collect_main` directly. So a detour on `_PyGC_Collect`
catches `gc.collect()` from Python but not `PyGC_Collect()` from C, and *vice versa*. The
three `_PyGC_Reason` values are `_Py_GC_REASON_HEAP`, `_Py_GC_REASON_SHUTDOWN`,
`_Py_GC_REASON_MANUAL` (`Include/internal/pycore_gc.h:133-142`), and only
`gc_collect_main` sees all three.

### Allocation path — why the exported allocators are a partial view

`_PyObject_GC_New` / `_PyObject_GC_NewVar` are exported, and core types do reach them
(`Objects/listobject.c:244`, `Objects/tupleobject.c:57`, `Objects/dictobject.c:877`,
`Objects/funcobject.c:114`, all via the `PyObject_GC_New` macro at
`Include/objimpl.h:180-183`). But **instances of Python-defined classes do not**:
`Objects/typeobject.c:2437-2439` calls `_PyObject_GC_Link(obj)` directly after its own
`_PyObject_MallocWithType`, bypassing `gc_alloc` entirely. So the union of the exported
allocators is a partial view of GC-tracked allocation, and the only complete choke point
is `_PyObject_GC_Link` — which is not exported.

Similarly for tracking: `PyObject_GC_Track` is exported, but almost all core code calls the
`static inline` `_PyObject_GC_TRACK` from `Include/internal/pycore_gc.h:207-235` instead
(`Objects/listobject.c:271,3996` uses the inline macro; only `:4151` uses the exported
function). An inline function in a header cannot be interposed at all — it has been copied
into every translation unit that used it.

---

## 6. Field-by-field: can a 3.14.5 hook reconstruct the 3.15 ring record?

Assume the best available hook, a `gc.callbacks` entry in a C extension (§8.1). The
callback fires at `Python/gc.c:1357-1359` (`"start"`) and `:1526-1528` (`"stop"`),
bracketing the entire collection.

| 3.15 ring field | 3.14.5 source | Reconstructible? |
|---|---|---|
| `ts_start` | Take `PyTime_PerfCounterRaw` in the `"start"` callback | **Yes**, with a small positive bias — 3.15 reads its clock *after* the start callbacks return (`3.15/Python/gc.c:1471` then `:1476`), so a callback-derived `ts_start` is earlier than the ring's by the cost of the callback list |
| `ts_stop` | `PyTime_PerfCounterRaw` in the `"stop"` callback | **Yes**, same bias in the other direction: 3.15 stamps `ts_stop` at `:1592`, *before* `invoke_gc_callback("stop")` at `:1643`. Combined error is the callback dispatch cost, ~a few µs |
| `duration` | `ts_stop - ts_start` | **Yes**, inflated by the above. 3.15 computes it identically (`3.15/Python/gc.c:1593`) |
| `collections` | `info["generation"]` at `"stop"` → increment your own per-gen counter. Cross-check against `gc.get_stats()[gen]["collections"]`, which 3.14.5 maintains at `Python/gc.c:1508-1509` | **Yes, exactly** |
| `collected` | `info["collected"]` at `"stop"` (`Python/gc.c:1527`, the `m` from `:1441,1466`) | **Yes, exactly** |
| `uncollectable` | `info["uncollectable"]` at `"stop"` (the `n` from `:1471-1475`) | **Yes, exactly** |
| `heap_size` | `interp->gc.heap_size` (`pycore_interp_structs.h:230-231`). Not exposed to Python. A C extension reaches it by including the internal headers with `Py_BUILD_CORE_MODULE`, or by a hardcoded offset. `Modules/_testinternalcapi.c:2354-2356` exposes it as `get_tracked_heap_size()` if that module is importable | **Yes on GIL builds**, at the cost of one layout dependency. **No on free-threaded builds** — the counter is never maintained there (§3) |
| `candidates` | **Nothing.** 3.14.5's `deduce_unreachable` is `static inline void` (`Python/gc.c:1112-1113`) and `update_refs` is `static void` (`:391-392`); neither counts. 3.15 changed `update_refs` to `static Py_ssize_t` returning the count (`3.15/Python/gc.c:396-401,433-435`) and plumbs it out through `deduce_unreachable` (`3.15/Python/gc.c:1176,1218`) | **No.** The only approximation is summing the lengths of generations 0..`gen` in the `"start"` callback, which is O(candidates) — it would roughly double the cost of a young-generation collection, and `"start"` fires *before* the younger generations are merged (`Python/gc.c:1381-1383`), so you would have to replicate the merge rule yourself. Not viable |

**Bottom line: 7 of 8 fields, with `heap_size` conditional on the build flavour and one
layout assumption, and `candidates` unobtainable.**

Two things a hook gets that the ring does not: the **reason**
(`_Py_GC_REASON_MANUAL` vs `_HEAP` — inferable from whether your own `PyGC_Collect`
wrapper or `gc.collect` was in the stack, though the callback itself is not told) and the
Python-level stack at collection time. 3.15 added `gcstate->frame` for the latter
(`3.15/…/pycore_interp_structs.h:242`); 3.14.5 has no equivalent.

---

## 7. Re-entrancy, GIL and free-threading safety

Whatever the hook, the same constraints apply, and they are visible in the source:

- **A collection cannot nest.** `gc_collect_main` opens with a compare-exchange on
  `gcstate->collecting` and returns 0 if it loses (`Python/gc.c:1331-1335`; identical at
  `gc_free_threading.c:2322-2326`). So a hook that calls `gc.collect()` re-entrantly is a
  silent no-op, not a crash — but it will corrupt your own bookkeeping.
- **The GIL is held for the whole of `gc_collect_main`** on GIL builds. A hook there runs
  under the GIL; blocking in it blocks the interpreter.
- **`gc.callbacks` run with an exception-clean state, and errors are swallowed.**
  `invoke_gc_callback` asserts `!_PyErr_Occurred` on entry and exit
  (`Python/gc.c:1205`, `:1250`), and any exception raised by a callback becomes
  `PyErr_FormatUnraisable("Exception ignored while calling GC callback %R", cb)`
  (`:1239-1242`). Your hook must not leave an exception set and must not rely on raising to
  report failure.
- **A `gc.callbacks` entry allocates.** `Py_BuildValue("{sisnsn}", …)` at `:1217-1220`
  builds a dict per invocation, and the callback is invoked with `PyObject_Vectorcall`
  (`:1238`) — so calling into Python-level callbacks allocates and can itself trigger the
  young-generation threshold. That is safe (the `collecting` flag blocks re-entry) but it
  means a Python callback perturbs the thing it measures. A C callback that only reads the
  dict and writes into a preallocated ring adds no allocations of its own.
- **Free-threaded builds run part of the collection with the world stopped**
  (`gc_free_threading.c:2189`, `:2267`) but both `invoke_gc_callback` calls
  (`:2343-2345`, `:2412-2414`) are *outside* the stopped-the-world regions, in
  `gc_collect_main`. So a callback there runs with other threads live. Any ring your hook
  writes must be written with the same discipline the 3.15 ring uses — publish the
  completion marker last (`3.15/Python/gc.c:1415-1417`) — and, on free-threaded builds,
  must tolerate a concurrent reader.
- **A detour on `_Py_RunGC` or `gc_collect_main` runs *inside* the collection**, where
  the object graph is in an inconsistent state for part of the time. Doing anything that
  allocates, or that touches tracked objects, is unsafe there in a way that a
  `gc.callbacks` entry is not. This is the strongest safety argument against approach 4/5.

---

## 8. The non-hook alternatives, on their merits

### 8.1 `gc.callbacks`

Documented at <https://docs.python.org/3.14/library/gc.html#gc.callbacks> —
`Doc/library/gc.rst:288-321` in-tree, `.. versionadded:: 3.3`. The doc names "Gathering
statistics about garbage collection, such as how often various generations are collected,
and how long the collection takes" as the *primary* intended use case (`:314-316`). This
is the supported answer to the question being asked.

What it gives on 3.14.5, from `Python/gc.c:1216-1220`:

```c
info = Py_BuildValue("{sisnsn}",
    "generation", generation,
    "collected", collected,
    "uncollectable", uncollectable);
```

— i.e. `generation` always; `collected` and `uncollectable` meaningful only on `"stop"`
(they are passed as `0, 0` on `"start"`, `:1358`). Phase is the first positional argument.
The list object is `gcstate->callbacks`, published to Python as `gc.callbacks` by
`Modules/gcmodule.c:546-548`, and initialised in `_PyGC_Init` (`Python/gc.c:142-145`), so
it exists before any user code runs.

What it misses versus the 3.15 ring: `candidates` (unobtainable, §6), `heap_size`
(obtainable out-of-band), `ts_start`/`ts_stop`/`duration` (obtainable by timing the two
invocations, with the small bias described in §6), and cumulative `collections` (obtainable
by counting, or read straight from `gc.get_stats()`, `Modules/gcmodule.c:365-380`).

Worth knowing for the 3.15 side of gcscope: **3.15 widened this callback**, and it now
carries `candidates` and `duration` directly (`3.15/Python/gc.c:1273-1278`). So a
callback-based collector written today degrades gracefully — on 3.15 it can read the two
extra keys instead of deriving them.

### 8.2 `sys.monitoring` and audit hooks

**No GC events exist in either.** Grepping `Include/cpython/monitoring.h` and
`Include/internal/pycore_instruments.h` for GC events returns nothing; the
`sys.monitoring` event set is entirely about code execution. The only `gc.*` audit events
are `gc.get_objects`, `gc.get_referrers` and `gc.get_referents`
(`Modules/gcmodule.c:252,300,337`; documented at `Doc/library/gc.rst:90,174,187`) — those
fire when someone *introspects* the heap, never when a collection runs.
`PySys_AddAuditHook` is exported from `python314.dll` (measured), but there is nothing for
it to observe here. Rule this out.

### 8.3 A C extension module that owns the ring

The recommendation. Shape:

- A `.pyd` that, at module exec, allocates a `static` (module-global, hence a fixed RVA in
  the extension's data section) ring shaped exactly like 3.15's
  `struct gc_stats` — `young[11]`, `old[2][3]`, `int8_t index` per ring
  (`3.15/…/pycore_interp_structs.h:198-222`).
- It appends a C callable to `gc.callbacks`. On `"start"` it stamps a thread-local
  `ts_start` and snapshots `heap_size`; on `"stop"` it fills the next ring entry with the
  cumulative-delta discipline of `add_stats` (`3.15/Python/gc.c:1399-1418`) and publishes
  `ts_stop` last.
- It exports one symbol, e.g. `gcshim_ring`, so an out-of-process reader can find the ring
  by walking the injected module's export table rather than by pattern-matching memory.

Why this beats detouring on every axis that matters here: no address discovery, no code
patching, no PDB dependency, no per-patch-release fragility, no work done inside the
collector's inconsistent-graph window, and it survives a CPython upgrade because
`gc.callbacks` is public API. The one thing it cannot do is `candidates`.

**Cost note:** this is *not* free. Every collection now pays a `Py_BuildValue` dict
construction, a `PyUnicode_FromString(phase)`, and a vectorcall
(`Python/gc.c:1217,1227,1238`) that it would otherwise skip: with an empty `gc.callbacks`
the dict is not built (`:1216` guards it) and the loop at `:1235` runs zero times. On a
young-generation-heavy workload that is a real, if small, tax, and
it is a tax the 3.15 ring does not levy. Measure it before claiming it is negligible; I
did not.

### 8.4 Backporting the 3.15 ring

Mechanically small, and the 3.14.5 struct has been left in a state that invites it. The
forward-port explicitly kept dead fields to preserve offsets
(`Include/internal/pycore_interp_structs.h:233-236`):

```c
    /* dummy members to preserve other offsets */
    Py_ssize_t dummy1; /* was work_to_do */
    int dummy2; /* was visited_space */
    int dummy3; /* was phase */
```

I verified by grep across every `.c` and `.h` in the tree that `dummy1`, `dummy2` and
`dummy3` are **declared and never read or written anywhere**. That is 16 contiguous bytes
of dead, offset-stable space inside `_gc_runtime_state`, zero-initialised, at a fixed
offset the reader already knows how to reach.

The backport is: change `struct gc_generation_stats` to the 3.15 shape, add
`gc_young_stats_buffer` / `gc_old_stats_buffer` / `gc_stats`, add `gc_get_stats` /
`gc_get_prev_stats` / `add_stats` (`3.15/Python/gc.c:1367-1418`), change `update_refs` and
`deduce_unreachable` to return the candidate count (`3.15/Python/gc.c:396-401,433-435`),
and rewrite the six `stats.*` assignments in `gc_collect_main`. `gc.get_stats()`
(`Modules/gcmodule.c:365-380`) needs adjusting because it copies the flat array. This is
the same change the author of this note landed upstream for 3.15
(<https://github.com/python/cpython/pull/146532>, "GH-146527: Add more data to GC
statistics and add it to PyDebugOffsets", merged to `main` 2026-03-28), so the diff
already exists in a reviewed form.

---

## 9. Out-of-process discoverability — the gcscope angle

This is the part that decides whether an injected ring is usable by gcscope at all.

**3.15's ring is self-describing.** `_Py_DebugOffsets` gained three fields for it
(`3.15/Include/internal/pycore_debug_offsets.h:229-236`, initialised at `:390-396`):

```c
    struct _gc {
        uint64_t size;
        uint64_t collecting;
        uint64_t frame;
        uint64_t generation_stats_size;
        uint64_t generation_stats;
    } gc;
```

CPython's own remote reader uses exactly those (`3.15/Modules/_remote_debugging/gc_stats.c:88-93`
computes `interpreter_state.gc + gc.generation_stats`, and `:127` reads
`gc.generation_stats_size`), and so does gcscope's ring path.

**3.14.5's is not.** Its block has only two fields
(`Include/internal/pycore_debug_offsets.h:210-213`, initialised at `:351-354`):

```c
    struct _gc {
        uint64_t size;
        uint64_t collecting;
    } gc;
```

There is no field an injected agent could fill in to advertise a ring — this is why
gcscope compiles the 3.14 `generation_stats` offset in
(`gcscope/src/remote_debugging/offsets/v_3_14_0.rs:646`, `GC_STATS_INLINE_OFF = 0x78`) and
can only warn rather than verify (`gcscope/CLAUDE.md`, "Fail-closed vs advisory"). So an
injected 3.14 ring **cannot** be discovered the way the 3.15 ring is. Three options,
best first:

1. **Publish through the injected module's export table.** The reader already parses PE/ELF
   exports (goblin) to find `_PyRuntime`; finding `gcshim_ring` in an injected DLL is the
   same operation against a different module. Self-describing, no CPython layout
   dependency, no writes to interpreter memory.
2. **Stash a pointer in `dummy1`.** 8 bytes, provably unused (§8.4), zero-initialised, at
   `offsetof(_gc_runtime_state, dummy1)` — a constant the reader can compile in exactly as
   it already compiles in `GC_STATS_INLINE_OFF`. A non-zero value there is unambiguously
   an injected marker, because nothing in CPython ever writes it. Cheap, but it writes into
   the interpreter's state, and it is a private handshake between two projects rather than
   a discoverable one.
3. Named shared memory / a well-known file. Works, but throws away the property that makes
   gcscope what it is — reading the target's own memory with nothing running inside it.

**Independent finding, worth a separate issue:** `GC_STATS_INLINE_OFF = 0x78` still holds
for 3.14.5. Computing `offsetof(struct _gc_runtime_state, generation_stats)` by hand from
`pycore_interp_structs.h:202-222` on x86-64 gives `trash_delete_later` 0, nesting/enabled/debug
8..20, pad to 24, `generations[3]` 24..96 (3 × 24), `permanent_generation` 96..120, so
`generation_stats` = 120 = `0x78`. The dummies did their job. **But `sizeof(struct
_gc_runtime_state)` did *not* survive**: 3.14.0's GIL branch ended at `int phase;` with
`long_lived_total`, `long_lived_pending` and `generation0` either absent or inside
`#ifdef Py_GIL_DISABLED` (`3.14.0/Include/internal/pycore_interp_structs.h`, and
`generation0` appears zero times in that file), whereas 3.14.5 has all three outside the
ifdef (`:244-248`, `:264`) — 24 bytes more. Since `gc.size` in `_Py_DebugOffsets` is
`sizeof(struct _gc_runtime_state)`, gcscope's `VERIFIED_GC_SIZES` membership check will
fire an advisory warning on 3.14.5 targets unless the sweep is re-run. That is a real,
already-shipped consequence of the forward-port, independent of anything in this note.

---

## 10. What I could not verify

1. **Free-threaded 3.14.5 exports.** No `python314t.dll` on this machine. The export
   partition is argued from `Include/exports.h:56-88`, which does not vary by build
   flavour, but it is not measured.
2. **Whether `gc_collect_main` survives as a distinct function in the shipped
   `python314.dll`.** It is `static` with four call sites; MSVC may inline or fold it. I
   did not disassemble. This is the load-bearing uncertainty under approach 5 — if it is
   inlined, that approach does not exist at all.
3. ~~**The cost of a `gc.callbacks` entry.**~~ **Measured 2026-08-05 — see §11.**
4. **Whether Microsoft's symbol server carries PDBs for this exact `python314.dll`.**
   Approach 4's address discovery depends on it; I did not check.
5. **`heap_size` semantics under sub-interpreters.** It is per-interpreter (it lives on
   `interp->gc`), but I did not trace whether objects shared between interpreters via
   single-phase-init extensions (the hazard `Python/gc.c:1763-1777` documents for GC head
   links) can make one interpreter's `heap_size` drift.
6. **The 3.15 ring's exact behaviour for generation ≥ 1 index wraparound** under the
   `GC_OLD_STATS_SIZE = 3` depth — I read `gc_get_stats`/`add_stats`
   (`3.15/Python/gc.c:1367-1418`) but did not test the loss-reconstruction implications,
   which are gcmon's territory ([`gcmon-inventory.md` §2.4](gcmon-inventory.md)).

---

## 11. Measured: what a `gc.callbacks` entry actually costs

Added 2026-08-05, resolving open question 3. Measured on this machine against
`C:\Python\Python314.5\python.exe` (`tags/v3.14.5:5607950`, MSC v.1944 x64), Windows 11,
GIL build. A purpose-built C extension (`gcprobe`, `METH_FASTCALL`, compiled `/O2 /MD`
against `Python314.5\libs\python314.lib`) supplied the C-callback variants and wrote a
3.15-shaped ring record from inside the callback.

`invoke_gc_callback` fires **twice per collection** — `"start"` at `Python/gc.c:1358`,
`"stop"` at `Python/gc.c:1527` — so per-collection cost covers two invocations.

### 11.1 Isolated overhead

Method: `gc.collect(0)` against an **empty** young generation, so baseline collection work
is ~0 and the delta is the callback path alone. Median of 15 batches × 400 collections.
Stable across two independent runs (±5%).

| Registered callback | ns / collection | overhead vs none | ns / invocation |
|---|---:|---:|---:|
| none | 230 | — | — |
| C no-op (`METH_FASTCALL`) | 560 | +330 | +165 |
| C, unpacks dict + writes ring | 888 | +658 | +329 |
| Python no-op (`def f(phase, info): pass`) | 641 | +411 | +206 |
| Python, unpacks dict + appends to deque | 905 | +675 | +338 |

**The Python↔C roundtrip is not the cost.** C no-op → Python no-op is +41 ns per
invocation (+82 ns per collection). The dominant term is the argument marshalling CPython
performs *before* dispatch — `Py_BuildValue("{sisnsn}", ...)` at `Python/gc.c:1217-1220`
plus the unconditional `PyUnicode_FromString(phase)` at `:1227` — roughly 165 ns, and **no
callback design avoids it**, because it happens above the `PyObject_Vectorcall` at `:1238`.

Corollary: writing the callback in C buys ~20% of the overhead, not an order of magnitude.
The C variant's advantage is not speed — it is that it can own a ring at a fixed address
(§8.3, §9) without a Python object graph in the way.

Note the ring-writing variant pays ~164 ns/invocation over the C no-op, of which a large
share is three `PyDict_GetItemString` calls (each re-hashes the key string) plus one
`QueryPerformanceCounter`. Interning the three keys once at module init would cut it.

### 11.2 As a fraction of a real collection

Method: fill the young gen to `threshold0` (2000 on this build) with unreachable cyclic
dicts, then time **only** the `gc.collect(0)` call, 400 samples, allocation excluded from
the timed region.

A realistic young-gen collection costs **~123 µs** (median; p10 ~120 µs). Against that, the
+658 ns worst-case measured above is **~0.5%** — and run-to-run drift of the 123 µs figure
is ±3 µs, i.e. **the overhead is four times smaller than the noise floor of its own
denominator**. A second run of the same benchmark put every configuration *below* baseline.

End-to-end confirmation, allocation-churn workload with automatic GC enabled (300k cyclic
dicts, 144 collections triggered, identical collection counts across all configurations):

| Registered callback | total | vs none |
|---|---:|---:|
| none | 50.3 ms | — |
| C no-op | 51.5 ms | +2.4% |
| C ring write | 51.1 ms | +1.6% |
| Python no-op | 51.4 ms | +2.2% |
| Python + unpack | 51.3 ms | +2.0% |

The predicted contribution is 144 × 658 ns ≈ 95 µs ≈ **0.19%** of 50 ms; the ~1 ms spread
observed is measurement noise on a 50 ms wall-clock, not callback cost. Read this table as
"under 1%, indistinguishable from noise", not as a 2% result.

### 11.3 Verdict

The roundtrip concern does not survive measurement. At 3.14.5's default thresholds a
`gc.callbacks` entry costs well under 1% of the collection it observes, whether written in
C or Python. It would only become interesting for a workload driving GC far harder than
the churn loop above, or if thresholds were tuned so low that collections are near-empty —
the regime in §11.1, where the fixed ~660 ns is most of the collection.

**Caveats.** GIL build only; the free-threaded build's `invoke_gc_callback`
(`Python/gc_free_threading.c:1874-1917`) performs the same marshalling but runs with the
world stopped (§7), which was not measured. Single-threaded process, no sub-interpreters.

Benchmark sources live in `X:\Work\gc-monitor\xxx\bench\` (`bench2.py` is the isolation
measurement above, `bench3.py` the realistic denominator). Re-runs vary ±30% on the
isolated per-invocation figures — the ordering and the C-vs-Python gap are stable, the
absolute nanoseconds are not.

---

## 12. Verified: gcscope reads an injected 3.14.5 ring unmodified

Added 2026-08-06. Approach 1 was built and read end to end. The producer is a C extension
(`gcprobe`) loaded into stock `C:\Python\Python314.5\python.exe`; the consumer is a Rust
binary depending on this repo by path, using **gcscope's own reader and decoder** —
`memory::reader::{open_handle, read_memory_h}` and `OffsetTable::decode_gc_stats`. No
decode logic was reimplemented.

### 12.1 What was built

`gcprobe` registers one `METH_FASTCALL` callback in `gc.callbacks` and mirrors 3.15's
region byte-for-byte:

- `struct gcprobe_generation_stats` — the 8 fields of `3.15/…/pycore_interp_structs.h:180-196`
  at offsets 0/8/16/24/32/40/48/56, `sizeof == 64`.
- Per-generation buffers `items[N]` + trailing `int8_t index` (`:205-213`) — that trailing
  byte, padded to 8, is exactly the inter-generation gap
  [`compute_ring_base_offsets`](../../src/remote_debugging/offsets/offset_table.rs) accounts for.
- `gcprobe_add_stats` reproduces `add_stats` (`3.15/Python/gc.c:1399-1418`): copy the
  previous entry forward, add this collection's deltas, **publish `ts_stop` last** behind a
  `MemoryBarrier()`.
- `gc_get_prev`/`gc_get_cur` reproduce the index advance of `3.15/Python/gc.c:1367-1397`.
- One region **per interpreter**, slots claimed with `InterlockedCompareExchange`, so the
  pending-`ts_start` scratch cannot be clobbered across a per-interpreter GIL.

Region geometry is pinned by `Py_BUILD_ASSERT` at compile time, so a layout drift is a
build failure rather than a silent misread.

### 12.2 `heap_size` — resolved, with a caveat

`heap_size` has no accessor. Including the internal headers requires `Py_BUILD_CORE`, which
flips `PyAPI_DATA` from `dllimport` to `dllexport` and breaks linking against
`python314.lib` (`unresolved external symbol _Py_NoneStruct`). The workable route is to
extract the offsets once with a separate `Py_BUILD_CORE` program that references no Python
data symbols, then hardcode them. Measured for 3.14.5 (x64, GIL build):

| Offset | Value |
|---|---:|
| `offsetof(PyInterpreterState, gc)` | 7400 |
| `offsetof(struct _gc_runtime_state, heap_size)` | 216 |
| `offsetof(struct _gc_runtime_state, collecting)` | 192 |
| `offsetof(struct _gc_runtime_state, generation_stats)` | **120 = 0x78** |
| `sizeof(struct _gc_runtime_state)` | **264** |

Two of these confirm earlier hand-computed claims in this note: `0x78` is
`GC_STATS_INLINE_OFF` (§9), and `sizeof == 264` confirms the growth versus 3.14.0 behind
the `VERIFIED_GC_SIZES` warning flagged in §9.

The extension self-checks these at runtime: `collecting` must read `1` inside a callback
(it is set at `3.14.5/Python/gc.c:1332` and cleared at `:1531`, bracketing both callbacks).
If it doesn't, `heap_size` reports 0 rather than garbage. In the run below the check
passed (`offsets_ok: 1`, `heap_size: 32853`). The self-check is what makes a stale constant
fail safe — and §13 shows it is the load-bearing protection, because the offsets turn out
to be stable where one would expect them to move.

### 12.3 The read

Target: 25 s of cyclic-dict churn, 1,984 rounds, with periodic `gc.collect()` and
`gc.collect(1)` to exercise all three rings. Three samples 1.5 s apart.

```
  item size      : gcprobe 64  gcscope 64
  gen base offs  : gcscope [0, 712, 912]
  buffer len     : gcprobe 1112  gcscope 1104
  MATCH (region is 8 bytes longer: gen-2 trailing index word)

decoded 17 entries (1104 bytes)
  written entries: 17   passing is_complete(): 17
    gen 0  entry  9  collections  1516  collected  671181  heap_size 29191  pause  133.9us
    gen 1  entry  2  collections   185  collected 2157986  heap_size 33687  pause 1634.2us
    gen 2  entry  2  collections    29  collected  513302  heap_size 19672  pause 3673.3us
  cumulative counters advanced: [1252, 153, 24] -> [1516, 185, 29]
ALL INVARIANTS HELD
```

Holding across all three samples: all 17 entries decoded; every written entry passed
`GcStat::is_complete()` (`ts_start < ts_stop`); every `duration` positive; cumulative
`collections` monotonically non-decreasing per generation; ring wraparound visible in
gen 0 (entry 10 carrying the oldest value once the 11-slot ring lapped).

**One geometry note worth keeping.** `OffsetTable::stats_buffer_len()` returns 1104 —
`bases[2] + entries[2] * item_size` — while `sizeof(struct gc_stats)` is 1112. gcscope
deliberately stops at the end of gen 2's entries and never reads gen 2's own trailing index
word. A producer region must therefore be **at least** 1104 bytes; 1112 is a correct
superset. Nothing to fix, but a hand-rolled 1104-byte region would also decode, and would
silently lose the gen-2 cursor.

### 12.4 Discovery — closed via the export table

The first version of this experiment passed the region address to the reader by hand,
because 3.14.5's `_Py_DebugOffsets` gc block has no `generation_stats` field to point at
the region (§9). That gap is now closed, and no debug-offsets change was needed.

The extension exports a `gcprobe_header` describing everything a reader needs: the
absolute address of the slot array, the slot stride, the offsets of `claimed`/`interp_id`/
`stats` within a slot, the item size, the ring depths and the region size. Resolution is
three steps, all from what the OS and the on-disk image already say:

1. Enumerate the target's mappings ([`memory::regions::list_regions`](../../src/memory/regions.rs))
   and find `gcprobe.pyd`; its first mapping is the PE load base.
2. Parse that file's PE export table with `goblin` — already a gcscope dependency — and
   take the RVA of `gcprobe_header`.
3. `base + rva` is the header. Validate the magic, then walk the slots.

Measured on a live target: header at `0x7ffd9eb85000`, one claimed slot, interpreter 0's
region at `0x7ffd9eb852f0`, geometry read back as `item_size 64, entries [11, 3, 3],
region 1112 bytes` — then decoded, with `ALL INVARIANTS HELD`. The reader's only input is
a **pid**. No scanning, no hardcoded address, no out-of-band geometry.

> **Byte-order trap, worth stating because it is silent.** The magic was first declared as
> `uint64_t magic = 0x4743505242303135ULL` — "GCPRB015" read big-endian. Stored
> little-endian, the bytes in memory are `510BRPCG`, and searching the image for the ASCII
> string finds *nothing* (verified: `find(b"GCPRB015")` returned −1 against the built
> `.pyd`, while the reversed form was at offset 7168). Declared as `char magic[8]` the
> bytes are literal and both symbol lookup and byte scanning work. Any magic intended to
> be greppable must be a char array.

Established overall: the region shape, the cumulative `add_stats` semantics, the
`ts_stop`-last publication order, discovery, and gcscope's decoder are mutually compatible.
A 3.14.5 process can present a ring that gcscope reads **with no changes to the decode
path**, found from a pid alone.

What a real gcscope integration still needs is the *plumbing*, not a new mechanism: the
export-table resolution above lives in the prototype reader, not in gcscope, and gcscope's
`PySession` currently reaches a region through `GcStatsRegion::{Direct, Deref}` off the
interpreter's gc state. An injected ring is a third shape — module-export-relative — so it
wants its own `GcStatsRegion` variant rather than a special case inside the existing two.

Also unmeasured here: free-threaded builds (rings are 1/1, and `heap_size` is never
maintained), and more than one interpreter actually running concurrently — the per-slot
design is argued, not exercised.

**Sources.** All of it lives in `X:\Work\gc-monitor\xxx\` — `gcprobe.c` (the extension),
`offsets.c` (re-derives the hardcoded offsets for a new patch release), `workload.py` (the
target), `verify/` (the Rust reader, depending on this repo by path), and `README.md` for
how to build and run. It is a prototype: single-file extension, hardcoded version-specific
offsets, no test suite.

---

## 13. Backporting the probe to 3.14.0 – 3.14.4

Added 2026-08-06. The question is whether the §12 prototype extends down the 3.14 line.
It does, with **one semantic hazard that is not a build problem** and would otherwise pass
unnoticed.

### 13.1 The Python-visible callback contract does not change

3.14.0–3.14.4 run the incremental collector, so the C plumbing differs — the callbacks are
invoked from `_PyGC_Collect` (`3.14.4/Python/gc.c:2044`, `:2073`) rather than
`gc_collect_main`, through a `do_gc_callback` that takes a `struct gc_collection_stats *`
(`:1804-1846`). None of that is visible from Python. The info dict is built by the same
`Py_BuildValue("{sisnsn}", "generation", …, "collected", …, "uncollectable", …)`
(`3.14.4/Python/gc.c:1813-1816`) and dispatched by the same two-argument
`PyObject_Vectorcall` (`:1834`). Both call sites carry the same
`reason != _Py_GC_REASON_SHUTDOWN` guard, so start/stop stay paired.

**The callback code needs no changes at all.**

### 13.2 The offsets are stable across the line — verified, not assumed

`offsets.c` compiled against each source tree (`offsets_from_source.bat`, which needs no
installed interpreter — it links without `pythonXX.lib` because it calls no Python
functions):

| | 3.14.0 | 3.14.4 | 3.14.5 |
|---|---:|---:|---:|
| `offsetof(PyInterpreterState, gc)` | 7400 | 7400 | 7400 |
| `_gc_runtime_state.heap_size` | 216 | 216 | 216 |
| `_gc_runtime_state.collecting` | 192 | 192 | 192 |
| `_gc_runtime_state.generation_stats` | 120 | 120 | 120 |
| `sizeof(_gc_runtime_state)` | 240 | 240 | **264** |

Every offset gcprobe uses is identical; only the struct's total size moved, and nothing
depends on it. `heap_size` exists on all three. The 3.14.5 column also cross-validates the
source-tree method against the installed-interpreter method — both produce 7400/216/192/120/264.

3.14.1–3.14.3 were not checked (no source trees on this machine), but they lie between two
agreeing endpoints.

### 13.3 The hazard: ring index 1 means two different things

On 3.14.0–3.14.4, `_PyGC_Collect` dispatches the `generation` argument as a **mode**
(`3.14.4/Python/gc.c:2056-2068`):

```c
switch(generation) {
    case 0: gc_collect_young(tstate, &stats);      break;
    case 1: gc_collect_increment(tstate, &stats);  break;   // <-- an INCREMENT
    case 2: gc_collect_full(tstate, &stats);       break;
}
```

Several increments make up one logical old-generation pass, so ring 1's `collections`
counts increments and its `collected` is per-increment. On 3.14.5 the same 0/1/2 are true
generations (`3.14.5/Python/gc.c:1337-1348`). Same ring, same field names, different
quantity — a consumer that pools 3.14.4 and 3.14.5 data silently compares unlike things,
and nothing fails to make it obvious.

The probe therefore publishes what it is observing rather than leaving the consumer to
infer it. Header v3 adds `py_version` (from `Py_Version`, the *runtime* version — not
`PY_VERSION_HEX`, which is what the `.pyd` was compiled against) and `collector`
(`0` = incremental, `1` = generational, selected on `micro >= 5`). The reader prints it:

```
  magic OK, header v3 (8 slots)
  host python  : 3.14.5
  collector    : generational (ring 1 counts gen-1 collections)
```

### 13.4 One binary, and what actually guards it

A single `.pyd` built against `python314.lib` loads into any 3.14.x — the ABI is stable
across a minor version — so no per-patch build is needed.

`PyInit_gcprobe` refuses to import on a non-3.14 minor version, but on Windows that gate is
belt-and-braces: the `.pyd` links `python314.dll`, so loading it into 3.15 fails at the
loader first (measured: `ImportError: DLL load failed while importing gcprobe`). The
load-bearing protection against an offset that moves *within* the 3.14 line remains the
runtime `collecting == 1` self-check, which degrades `heap_size` to 0 instead of publishing
whatever sits at the stale address.

### 13.5 Not verified

3.14.0–3.14.4 were checked **by source inspection and offset computation only**. Neither
was run: the machine has no 3.14.0 build, and `C:\Python\Python314.4` is a stub (`Doc`,
`Lib`, `Scripts` — no `python.exe`, no headers, no `libs`). Every §12 runtime result is
3.14.5. Re-running `workload.py` + the reader against a real 3.14.4 build is the one
outstanding check, and the thing most likely to surface is §13.3 rather than a crash.

---

## 14. Deepening the rings

Added 2026-08-06. An injected ring need not use 3.15's depths. What that costs is worth
knowing before doing it.

### 14.1 Why you would

Ring depth buys **per-collection fidelity only**. Because records are cumulative (§2), a
reader that misses entries still recovers `collections`/`collected`/`uncollectable`/
`duration` by differencing any two surviving records — what is lost is the individual
`ts_start`/`ts_stop`/`heap_size` of skipped collections.

At the young-gen rate measured in §12's churn workload — collections advancing 1252 → 1516
across a 1.5 s sample, ≈176/s — an 11-slot young ring **laps in about 62 ms**. Any poll
interval above that drops most individual pause records while leaving the totals intact.
512 slots stretches that to ≈2.9 s.

### 14.2 The `int8_t` cap, and why widening is free

3.15 declares the per-buffer cursor `int8_t index` (`3.15/…/pycore_interp_structs.h:205-213`),
which caps a ring at **128 entries** — the index must hold `SIZE-1`, and 128+ overflows.

Widening it to `int64_t` removes the cap at **zero layout cost**: the buffer is 8-byte
aligned either way, so `items[N]` is still followed by exactly 8 bytes and every base
offset `compute_ring_base_offsets` produces is unchanged. gcscope cannot observe the
difference — [`offset_table.rs:746`](../../src/remote_debugging/offsets/offset_table.rs)
states plainly that the trailing cursor is "the trailing 8-byte cursor the decoder never
reads", and nothing in `src/` reads it.

### 14.3 The real cost: gcscope's native path hardcodes 11/3

[`offsets/mod.rs:663-682`](../../src/remote_debugging/offsets/mod.rs) fixes the depths:

```rust
let (young, old) = if free_threaded != 0 { (1u64, 1u64) } else { (11, 3) };
```

This is not laziness — the depths are **not inferable** from what the process reports. The
region size gives one equation, `(young + 2*old) * item_size + 24`, in two unknowns. So any
reader of a non-3.15-shaped ring must be *told* the depths.

That is exactly what the probe's header carries (`young_entries`, `old_entries`), and the
prototype reader already builds its `OffsetTable` from those rather than from `set_ring`.
The consequence is a sharpening of §12.4's open item rather than a new one: an injected
ring wants its own `GcStatsRegion` variant **and** its own geometry source. Keep the
defaults at 11/3 and an injected ring stays byte-identical to a native 3.15 one; deepen it
and the header becomes load-bearing.

### 14.4 Verified

Depths are a build-time knob (`build.bat [young] [old]`), defaulting to 11/3. Built at
512/128 and read out of a live process:

```
  geometry     : item_size 64  entries [512, 128, 128]  region 49176 bytes
  gen base offs  : gcscope [0, 32776, 40976]
  buffer len     : gcprobe 49176  gcscope 49168
  decoded 768 entries (49168 bytes)
  written entries: 664   passing is_complete(): 664
ALL INVARIANTS HELD
```

Base offsets scale correctly, the 8-byte trailing-cursor relationship holds, and the
`Py_BUILD_ASSERT` block is now expressed in terms of the depth macros, so a rebuilt ring
re-checks its own geometry at compile time. Memory is 64 bytes/entry/interpreter: 512/128
is ~49 KB per interpreter, ~393 KB across all 8 slots. The default build was restored to
11/3.

---

## Appendix: symbols checked by name, as requested

| Asked about | Exists in 3.14.5? | Where |
|---|---|---|
| `PyGC_Collect` | yes, public, exported | `Python/gc.c:1671`; `gc_free_threading.c:2649` |
| `_PyGC_Collect` | yes, internal, **not exported** | `Python/gc.c:1689`; `gc_free_threading.c:2667`; decl `pycore_gc.h:318` |
| `gc_collect_main` | yes, **static** | `Python/gc.c:1313`; `gc_free_threading.c:2310` |
| `gc_collect_generation` | **no** | — |
| `gc_collect_increment` | **no in 3.14.5** — present in 3.14.4 at `3.14.4/Python/gc.c:1614` | — |
| `gc_collect_full` | **no in 3.14.5** — present in 3.14.4 at `3.14.4/Python/gc.c:1674` | — |
| `gc_collect_young` / `gc_collect_region` | **no in 3.14.5** — present in 3.14.4 at `:1325`, `:1280`/`:1707` | — |
| `gc_collect_internal` | **free-threaded only**, static | `gc_free_threading.c:2187` |
| `invoke_gc_callback` | yes, **static**, both builds | `Python/gc.c:1201`; `gc_free_threading.c:1874` |
| `_PyObject_GC_Link` | yes, internal, **not exported** | `Python/gc.c:1855`; `gc_free_threading.c:2804`; decl `pycore_object.h:865` |
| `_PyObject_GC_New` / `_NewVar` | yes, public, exported | `Python/gc.c:1907`, `:1926` |
| `PyObject_GC_Del` | yes, public, exported | `Python/gc.c:1978`; `gc_free_threading.c:2911` |
| `_PyObject_GC_Alloc` | **no** — the name is `gc_alloc`, and it is static | `Python/gc.c:1887`; `gc_free_threading.c:2819` |
| `_Py_ScheduleGC` | yes, internal, **not exported** | `Python/gc.c:1846`; `gc_free_threading.c:2795` |
| `_PyEval_AddPendingCall` | yes, and **exported** — it carries `PyAPI_FUNC` with the comment `// Export for '_testinternalcapi' shared extension` (`pycore_ceval.h:60-65`). Not on the GC path: the GC uses the eval-breaker bit, not a pending call | — |
| `PyUnstable_GC_VisitObjects` | yes, public, exported — but never invoked by the collector; it is a caller-driven heap walk that disables GC for its duration (`Python/gc.c:2044-2046`, doc `Doc/c-api/gcsupport.rst:322-335`) | `Python/gc.c:2042` |
| `_PyGC_Freeze` / `_Unfreeze` / `_GetFreezeCount` | yes, internal, **not exported** | `Python/gc.c:1619`, `:1629`, `:1637`; decls `pycore_gc.h:321-326` |
| `gc.callbacks` mechanism | yes — list on `gcstate->callbacks`, created `Python/gc.c:142-145`, exposed `Modules/gcmodule.c:546-548`, invoked `Python/gc.c:1358` and `:1527` | — |
| `struct gc_collection_stats` | **declared and entirely unused** — `pycore_interp_structs.h:181-186`, zero references anywhere in the tree. Vestigial from the incremental collector | — |
