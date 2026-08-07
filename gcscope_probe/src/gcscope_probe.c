/* gcscope_probe -- publish a 3.15-shaped GC statistics Ring from inside CPython 3.14.
 *
 * CPython below 3.15 records `collections`, `collected` and `uncollectable` per generation
 * and no timestamps, so gcscope can say how often the collector ran but not what any
 * Collection cost. A `gc.callbacks` entry here writes a region mirroring 3.15's
 * `struct gc_stats` byte-for-byte (3.15/Include/internal/pycore_interp_structs.h:180-222),
 * which gcscope's ring decoder reads unmodified.
 *
 * Scope today: 3.14 only, x86-64 only, and the interpreter offsets below come from a WINDOWS
 * build. The code compiles and runs on Linux, but nobody has re-derived that constant there
 * and `PyInterpreterState` differs in layout across platforms. Expect the self-check to fail
 * on Linux and `heap_size` to read 0 until ticket 03 compiles the offsets in.
 * `specs/0013-probe-portable-core.md` covers what each becomes.
 */
#define PY_SSIZE_T_CLEAN
#include <Python.h>
#include <stdatomic.h>
#include <stddef.h>   /* offsetof -- came in via <windows.h> before the port */
#include <stdint.h>
#include <string.h>

/* Default visibility for the two symbols an out-of-process reader looks up. `PyMODINIT_FUNC`
 * carries it; `gcscope_probe_header` and the slot array do not, and drop out of the dynamic
 * symbol table under `-fvisibility=hidden` with no diagnostic. Discovery then finds nothing.
 * `tests/probe.rs` looks the symbol up in `.dynsym` only, so a regression here fails a test
 * instead of shipping. */
#ifdef _WIN32
#  define GCSCOPE_PROBE_EXPORT __declspec(dllexport)
#else
#  define GCSCOPE_PROBE_EXPORT __attribute__((visibility("default")))
#endif

/* `heap_size` lives in the internal `_gc_runtime_state` and has no accessor. Including the
 * internal headers directly requires Py_BUILD_CORE, which flips PyAPI_DATA from dllimport to
 * dllexport and breaks linking against python314.lib (_Py_NoneStruct). So the offsets are
 * transcribed here.
 *
 * Verified IDENTICAL across 3.14.0, 3.14.4 and 3.14.5 (x64, GIL build) by compiling a
 * throwaway translation unit WITH Py_BUILD_CORE against each source tree:
 *     offsetof(PyInterpreterState, gc)              = 7400
 *     offsetof(struct _gc_runtime_state, heap_size) = 216
 *     offsetof(struct _gc_runtime_state, collecting)= 192
 * Only sizeof(_gc_runtime_state) moved (240 on 3.14.0-3.14.4, 264 on 3.14.5), and nothing
 * here depends on it. 3.14.1-3.14.3 were not checked (no source tree available), but they sit
 * between two agreeing endpoints.
 *
 * `specs/0013-probe-portable-core.md` §4 makes that throwaway TU permanent, compiled with
 * Py_BUILD_CORE, turning these three constants into compile-time facts of the interpreter
 * being built against. Until then the module stays 3.14-only: PyInit refuses any other minor
 * version, and `collecting` self-checks the other two against a live collection. */
/* What Ring index 1 means, which is NOT the same across the 3.14 line.
 *
 * 3.14.0-3.14.4 run the incremental collector: `_PyGC_Collect` dispatches on the
 * `generation` argument as a MODE -- 0 = young, 1 = *increment* of the old space,
 * 2 = full (3.14.4/Python/gc.c:2056-2068). Several increments make up one logical
 * old-generation pass, so Ring 1's `collections` counts increments and its `collected`
 * is per-increment.
 *
 * 3.14.5 forward-ported the generational collector, so 0/1/2 are true generations
 * (3.14.5/Python/gc.c:1337-1348). Same Ring, same field names, different meaning. Average
 * across the two and you compare unlike quantities with nothing to warn you, which is why
 * the header publishes which one this is. */
#define GCSCOPE_PROBE_COLLECTOR_INCREMENTAL  0
#define GCSCOPE_PROBE_COLLECTOR_GENERATIONAL 1

#define GCSCOPE_PROBE_INTERP_GC_OFF        7400
#define GCSCOPE_PROBE_GC_HEAP_SIZE_OFF     216
#define GCSCOPE_PROBE_GC_COLLECTING_OFF    192

/* ---- 3.15 region layout, reproduced exactly ----------------------------- */

/* Ring depths. Defaults match 3.15 exactly (GIL build; free-threaded is 1/1), so the region
 * is byte-identical to a Native 3.15 one. Override at build time with
 *     CFLAGS=-DGCSCOPE_PROBE_YOUNG_STATS_SIZE=512 ...
 *
 * Depth buys per-Collection fidelity. The other fields are cumulative and survive wrap-around
 * by differencing. At the young-generation rate measured on this machine, ~176 collections/s
 * under churn, an 11-slot Ring laps in about 62 ms, so a poll interval above that drops most
 * individual pause Records. Cost is 64 bytes per entry per interpreter. */
#ifndef GCSCOPE_PROBE_YOUNG_STATS_SIZE
#  define GCSCOPE_PROBE_YOUNG_STATS_SIZE 11
#endif
#ifndef GCSCOPE_PROBE_OLD_STATS_SIZE
#  define GCSCOPE_PROBE_OLD_STATS_SIZE   3
#endif
#define GC_YOUNG_STATS_SIZE GCSCOPE_PROBE_YOUNG_STATS_SIZE
#define GC_OLD_STATS_SIZE   GCSCOPE_PROBE_OLD_STATS_SIZE
#define NUM_GENERATIONS     3

/* 3.15/Include/internal/pycore_interp_structs.h:180-196. 64 bytes, 8-byte aligned. */
struct gcscope_probe_generation_stats {
    int64_t    ts_start;        /*  0 */
    int64_t    ts_stop;         /*  8  -- published LAST, see gcscope_probe_add_stats */
    Py_ssize_t collections;     /* 16  cumulative */
    Py_ssize_t collected;       /* 24  cumulative */
    Py_ssize_t uncollectable;   /* 32  cumulative */
    Py_ssize_t candidates;      /* 40  cumulative -- NOT reconstructible on 3.14 */
    double     duration;        /* 48  cumulative seconds */
    Py_ssize_t heap_size;       /* 56  per-collection snapshot */
};

/* :205-213 -- the trailing index is what pads each buffer by 8 bytes, which is the
 * inter-generation gap gcscope's compute_ring_base_offsets() accounts for.
 *
 * 3.15 declares this `int8_t`, which caps a Ring at 128 entries: the index must hold SIZE-1,
 * and 128+ overflows. Widening it to int64_t removes the cap for FREE -- the struct is 8-byte
 * aligned either way, so `items[N]` is still followed by exactly 8 bytes and every base offset
 * is unchanged. gcscope never reads the field (`offset_table.rs`: "the trailing 8-byte cursor
 * the decoder never reads"), so nothing downstream can tell the difference. */
struct gcscope_probe_young_stats_buffer {
    struct gcscope_probe_generation_stats items[GC_YOUNG_STATS_SIZE];
    int64_t index;
};
struct gcscope_probe_old_stats_buffer {
    struct gcscope_probe_generation_stats items[GC_OLD_STATS_SIZE];
    int64_t index;
};
struct gcscope_probe_stats {                /* :219-222 */
    struct gcscope_probe_young_stats_buffer young;
    struct gcscope_probe_old_stats_buffer   old[2];
};

/* ---- per-interpreter state ---------------------------------------------- */

/* 3.15 hangs one `struct gc_stats` off each interpreter's gc state, so each interpreter gets
 * its own region here too. That removes the cross-interpreter race on the pending ts_start
 * entirely -- no shared scratch to clobber under a per-interpreter GIL. */
#define GCSCOPE_PROBE_MAX_INTERP 8

typedef struct {
    atomic_int                  claimed;    /* 0 = free, 1 = owned by interp_id */
    int64_t                     interp_id;
    struct gcscope_probe_stats  stats;
    /* scratch carried from the "start" callback to the "stop" callback */
    int64_t                     pending_ts_start;
    Py_ssize_t                  pending_heap_size;
    int                         pending_valid;
} gcscope_probe_slot;

/* Exported so an out-of-process reader can locate the regions from this module's own symbol
 * table -- this module's, not the interpreter's. */
GCSCOPE_PROBE_EXPORT gcscope_probe_slot gcscope_probe_slots[GCSCOPE_PROBE_MAX_INTERP];

/* The discovery anchor. Everything an out-of-process reader needs to walk the slots and decode
 * a region lives here, so nothing has to be passed in out of band and no struct layout is
 * hardcoded on the reader side.
 *
 * THE MODULE FILENAME IS A WIRE CONTRACT. gcscope discovers a Probe by enumerating the
 * target's mapped images, keeping those whose basename starts with `gcscope_probe`, and
 * looking up `gcscope_probe_header` in the export table (ADR 0014). Rename the module or this
 * symbol and discovery breaks in every gcscope already released, with no error to read: the
 * renamed module still loads, still installs its callback, still publishes a valid region
 * nothing can find. Rename only with a header version bump and a matching reader change.
 *
 * `magic` is a char array, NOT a uint64_t literal: a `uint64_t` constant is stored
 * little-endian, so 0x4743505242303135 ("GCPRB015" read big-endian) lands in memory as the
 * bytes "510BRPCG" and a scanner looking for the obvious ASCII string finds nothing. As a
 * char[8] the bytes are literal and both a symbol lookup and a byte scan work.
 *
 * Not const: `slots_addr` and `header_size` are filled at module init. An address is not
 * portably a static initializer once cast to an integer. */
GCSCOPE_PROBE_EXPORT struct gcscope_probe_header_t {
    char     magic[8];             /* "GCPRB015", no NUL -- exactly 8 bytes */
    uint32_t header_size;          /* sizeof(this struct); lets the reader version-check */
    uint32_t version;              /* bump on any layout change below */
    uint64_t slots_addr;           /* absolute address of gcscope_probe_slots in this process */
    uint32_t max_interp;           /* number of slots to walk */
    uint32_t slot_stride;          /* sizeof(gcscope_probe_slot) */
    uint32_t slot_claimed_off;     /* offsetof(slot, claimed)  -- 0 = unused slot */
    uint32_t slot_interp_id_off;   /* offsetof(slot, interp_id) */
    uint32_t stats_off_in_slot;    /* offsetof(slot, stats) */
    uint32_t item_size;            /* sizeof(struct gcscope_probe_generation_stats) */
    uint32_t region_size;          /* sizeof(struct gcscope_probe_stats) */
    uint32_t young_entries;        /* GC_YOUNG_STATS_SIZE */
    uint32_t old_entries;          /* GC_OLD_STATS_SIZE */
    uint32_t py_version;           /* Py_Version of the host, e.g. 0x030E0500 */
    uint32_t collector;            /* GCSCOPE_PROBE_COLLECTOR_* -- what Ring index 1 MEANS */
} gcscope_probe_header = {
    {'G', 'C', 'P', 'R', 'B', '0', '1', '5'},
    0,                              /* header_size  -- set in PyInit_gcscope_probe */
    3,
    0,                              /* slots_addr   -- set in PyInit_gcscope_probe */
    GCSCOPE_PROBE_MAX_INTERP,
    (uint32_t)sizeof(gcscope_probe_slot),
    (uint32_t)offsetof(gcscope_probe_slot, claimed),
    (uint32_t)offsetof(gcscope_probe_slot, interp_id),
    (uint32_t)offsetof(gcscope_probe_slot, stats),
    (uint32_t)sizeof(struct gcscope_probe_generation_stats),
    (uint32_t)sizeof(struct gcscope_probe_stats),
    GC_YOUNG_STATS_SIZE, GC_OLD_STATS_SIZE,
    0,                              /* py_version -- set in PyInit_gcscope_probe */
    0                               /* collector  -- set in PyInit_gcscope_probe */
};

static gcscope_probe_slot *
gcscope_probe_slot_for_current_interp(void)
{
    PyInterpreterState *interp = PyInterpreterState_Get();
    if (interp == NULL) {
        return NULL;
    }
    int64_t id = PyInterpreterState_GetID(interp);

    for (int i = 0; i < GCSCOPE_PROBE_MAX_INTERP; i++) {
        if (gcscope_probe_slots[i].claimed && gcscope_probe_slots[i].interp_id == id) {
            return &gcscope_probe_slots[i];
        }
    }
    /* Claim a free slot. Atomic because two interpreters with separate GILs can reach here
     * concurrently -- the GIL does not serialise them. */
    for (int i = 0; i < GCSCOPE_PROBE_MAX_INTERP; i++) {
        int expected = 0;
        if (atomic_compare_exchange_strong(&gcscope_probe_slots[i].claimed, &expected, 1)) {
            gcscope_probe_slots[i].interp_id = id;
            return &gcscope_probe_slots[i];
        }
    }
    return NULL;  /* more than GCSCOPE_PROBE_MAX_INTERP interpreters -- drop, don't corrupt */
}

/* ---- the Ring writer, mirroring 3.15's add_stats ------------------------ */

/* Total Records published. Not part of the 3.15 shape; a liveness counter for the benchmarks.
 * Never the correctness gate, which is the region read from outside. */
static volatile long long gcscope_probe_records_written = 0;

static struct gcscope_probe_generation_stats *
gcscope_probe_get_prev(struct gcscope_probe_stats *s, int gen)
{
    if (gen == 0) {
        return &s->young.items[s->young.index];
    }
    return &s->old[gen - 1].items[s->old[gen - 1].index];
}

static struct gcscope_probe_generation_stats *
gcscope_probe_get_cur(struct gcscope_probe_stats *s, int gen)
{
    if (gen == 0) {
        s->young.index = (s->young.index + 1) % GC_YOUNG_STATS_SIZE;
        return &s->young.items[s->young.index];
    }
    struct gcscope_probe_old_stats_buffer *b = &s->old[gen - 1];
    b->index = (b->index + 1) % GC_OLD_STATS_SIZE;
    return &b->items[b->index];
}

/* 3.15/Python/gc.c:1399-1418. Records are CUMULATIVE: the previous entry is copied forward and
 * this Collection's deltas added, so a remote reader can difference any two entries and
 * reconstruct Collections it never sampled. */
static void
gcscope_probe_add_stats(struct gcscope_probe_stats *s, int gen,
                        int64_t ts_start, int64_t ts_stop, double duration,
                        Py_ssize_t collected, Py_ssize_t uncollectable,
                        Py_ssize_t heap_size)
{
    struct gcscope_probe_generation_stats *prev = gcscope_probe_get_prev(s, gen);
    struct gcscope_probe_generation_stats *cur  = gcscope_probe_get_cur(s, gen);

    memcpy(cur, prev, sizeof(*cur));

    cur->ts_start       = ts_start;
    cur->collections   += 1;
    cur->collected     += collected;
    cur->uncollectable += uncollectable;
    cur->candidates    += 0;      /* deduce_unreachable() is static inline on 3.14 */
    cur->duration      += duration;
    cur->heap_size      = heap_size;

    /* Publish ts_stop last so a remote reader never selects a torn Record as newest
     * (3.15/Python/gc.c:1415-1417). The fence keeps the compiler and CPU from hoisting this
     * store above the field writes above.
     *
     * A release FENCE followed by a plain store establishes this on x86-64's TSO and NOT on
     * aarch64. That is a correctness defect no supported platform can exhibit yet, and the
     * port left it alone on purpose: `specs/0013-probe-portable-core.md` §4 replaces it with
     * an explicit release store on `ts_stop`, paired there with the native arm64 leg that
     * would fail without it. Fixing it on an x86-64 leg would land the change where nothing
     * can show it working. */
    atomic_thread_fence(memory_order_release);
    cur->ts_stop = ts_stop;

    gcscope_probe_records_written++;
}

/* ---- heap_size ---------------------------------------------------------- */

/* Set to 1 once the offsets have been confirmed against a live collection; -1 if the
 * self-check failed, in which case heap_size reports 0 rather than garbage.
 *
 * Reachable only through `geometry()`, which an out-of-process reader cannot call, so a
 * failed self-check looks like success to the one consumer that matters. It moves into the
 * header's capability word in spec 0013 §4. */
static int gcscope_probe_offsets_ok = 0;

static char *
gcscope_probe_gcstate(void)
{
    PyInterpreterState *interp = PyInterpreterState_Get();
    if (interp == NULL) {
        return NULL;
    }
    return (char *)interp + GCSCOPE_PROBE_INTERP_GC_OFF;
}

/* Called from inside a gc callback, where gcstate->collecting is necessarily 1 (set at
 * 3.14.5/Python/gc.c:1332, cleared at :1531 -- both callbacks fire between those points). If
 * it doesn't read 1, GCSCOPE_PROBE_INTERP_GC_OFF is wrong for this build and every other
 * offset-derived read is garbage.
 *
 * This validates the `gc` and `collecting` offsets JOINTLY, so a heap_size-only move passes
 * it and then publishes plausible garbage. Spec 0013 §4 gives heap_size its own check. */
static void
gcscope_probe_check_offsets(void)
{
    if (gcscope_probe_offsets_ok != 0) {
        return;
    }
    char *gcstate = gcscope_probe_gcstate();
    if (gcstate == NULL) {
        return;
    }
    int collecting = *(int *)(gcstate + GCSCOPE_PROBE_GC_COLLECTING_OFF);
    gcscope_probe_offsets_ok = (collecting == 1) ? 1 : -1;
}

static Py_ssize_t
gcscope_probe_heap_size(void)
{
    /* 3.14 GIL build only -- the free-threaded build never maintains heap_size. */
    if (gcscope_probe_offsets_ok != 1) {
        return 0;
    }
    char *gcstate = gcscope_probe_gcstate();
    if (gcstate == NULL) {
        return 0;
    }
    return *(Py_ssize_t *)(gcstate + GCSCOPE_PROBE_GC_HEAP_SIZE_OFF);
}

/* ---- the gc.callbacks entry point --------------------------------------- */

static PyObject *
gcscope_probe_on_gc(PyObject *self, PyObject *const *args, Py_ssize_t nargs)
{
    (void)self;
    if (nargs != 2) {
        Py_RETURN_NONE;
    }
    PyObject *phase = args[0];
    PyObject *info  = args[1];

    gcscope_probe_slot *slot = gcscope_probe_slot_for_current_interp();
    if (slot == NULL) {
        Py_RETURN_NONE;
    }

    /* "start" vs "stop" -- differ first at index 2 ('a' vs 'o') */
    int is_stop = 0;
    if (PyUnicode_Check(phase) && PyUnicode_GET_LENGTH(phase) > 2) {
        is_stop = (PyUnicode_READ_CHAR(phase, 2) == 'o');
    }

    PyTime_t now = 0;
    (void)PyTime_PerfCounterRaw(&now);

    if (!is_stop) {
        /* 3.15 snapshots heap_size and ts_start here (gc.c:1474-1476). Ours runs a few
         * hundred ns earlier -- inside the callback rather than after it. */
        gcscope_probe_check_offsets();
        slot->pending_heap_size = gcscope_probe_heap_size();
        slot->pending_ts_start  = (int64_t)now;
        slot->pending_valid     = 1;
        Py_RETURN_NONE;
    }

    if (!slot->pending_valid) {
        Py_RETURN_NONE;   /* stop without a start: shutdown, or we loaded mid-collection */
    }
    slot->pending_valid = 0;

    int generation = -1;
    Py_ssize_t collected = 0, uncollectable = 0;
    if (info != NULL && PyDict_Check(info)) {
        PyObject *v;
        if ((v = PyDict_GetItemString(info, "generation")) != NULL) {
            generation = (int)PyLong_AsLong(v);
        }
        if ((v = PyDict_GetItemString(info, "collected")) != NULL) {
            collected = PyLong_AsSsize_t(v);
        }
        if ((v = PyDict_GetItemString(info, "uncollectable")) != NULL) {
            uncollectable = PyLong_AsSsize_t(v);
        }
    }
    if (generation < 0 || generation >= NUM_GENERATIONS) {
        if (PyErr_Occurred()) {
            PyErr_Clear();
        }
        Py_RETURN_NONE;
    }

    int64_t ts_start = slot->pending_ts_start;
    int64_t ts_stop  = (int64_t)now;
    /* Same conversion 3.15 uses (3.15/Python/gc.c:1593), rather than open-coding the
     * nanosecond scale. PyAPI_FUNC in Include/cpython/pytime.h:14, so it is public and
     * exported from python314.dll -- no internal API needed. */
    double duration = PyTime_AsSecondsDouble((PyTime_t)(ts_stop - ts_start));

    gcscope_probe_add_stats(&slot->stats, generation, ts_start, ts_stop, duration,
                            collected, uncollectable, slot->pending_heap_size);

    if (PyErr_Occurred()) {
        PyErr_Clear();
    }
    Py_RETURN_NONE;
}

/* ---- benchmark-only entry point ----------------------------------------- */

/* Registered in gc.callbacks by bench/*.py to isolate the cost CPython pays to marshal the
 * callback arguments and dispatch, with no work of our own on top. The difference between
 * this and `on_gc` is the cost of writing a Record. */
static PyObject *
gcscope_probe_noop(PyObject *self, PyObject *const *args, Py_ssize_t nargs)
{
    (void)self; (void)args; (void)nargs;
    Py_RETURN_NONE;
}

/* ---- in-process introspection ------------------------------------------- */

/* For liveness and the benchmarks. None is a correctness gate: the Probe's output is the
 * region, and asserting through a Python call would test the one surface no consumer uses
 * (spec 0013 §5). */

static PyObject *
gcscope_probe_install(PyObject *self, PyObject *noargs)
{
    (void)self; (void)noargs;
    PyObject *gc = PyImport_ImportModule("gc");
    if (gc == NULL) {
        return NULL;
    }
    PyObject *cbs = PyObject_GetAttrString(gc, "callbacks");
    Py_DECREF(gc);
    if (cbs == NULL) {
        return NULL;
    }
    PyObject *mod = PyImport_ImportModule("gcscope_probe");
    if (mod == NULL) {
        Py_DECREF(cbs);
        return NULL;
    }
    PyObject *fn = PyObject_GetAttrString(mod, "on_gc");
    Py_DECREF(mod);
    if (fn == NULL) {
        Py_DECREF(cbs);
        return NULL;
    }
    int rc = PyList_Append(cbs, fn);
    Py_DECREF(fn);
    Py_DECREF(cbs);
    if (rc < 0) {
        return NULL;
    }
    Py_RETURN_NONE;
}

static PyObject *
gcscope_probe_region_addr(PyObject *self, PyObject *noargs)
{
    (void)self; (void)noargs;
    gcscope_probe_slot *slot = gcscope_probe_slot_for_current_interp();
    if (slot == NULL) {
        Py_RETURN_NONE;
    }
    return PyLong_FromVoidPtr(&slot->stats);
}

static PyObject *
gcscope_probe_geometry(PyObject *self, PyObject *noargs)
{
    (void)self; (void)noargs;
    return Py_BuildValue(
        "{sksIsIsIsIsIsIsIsn}",
        "header_addr",     (unsigned long long)(uintptr_t)&gcscope_probe_header,
        "item_size",       (unsigned int)sizeof(struct gcscope_probe_generation_stats),
        "region_size",     (unsigned int)sizeof(struct gcscope_probe_stats),
        "young_entries",   (unsigned int)GC_YOUNG_STATS_SIZE,
        "old_entries",     (unsigned int)GC_OLD_STATS_SIZE,
        "young_index_off", (unsigned int)offsetof(struct gcscope_probe_stats, young.index),
        "old0_off",        (unsigned int)offsetof(struct gcscope_probe_stats, old),
        "offsets_ok",      (unsigned int)(gcscope_probe_offsets_ok == 1),
        "heap_size",       gcscope_probe_heap_size());
}

static PyObject *
gcscope_probe_records(PyObject *self, PyObject *noargs)
{
    (void)self; (void)noargs;
    return PyLong_FromLongLong(gcscope_probe_records_written);
}

static PyMethodDef gcscope_probe_methods[] = {
    {"on_gc", (PyCFunction)(void(*)(void))gcscope_probe_on_gc, METH_FASTCALL,
     "gc.callbacks entry: writes a 3.15-shaped Ring Record"},
    {"noop", (PyCFunction)(void(*)(void))gcscope_probe_noop, METH_FASTCALL,
     "gc.callbacks entry that does nothing (benchmark baseline)"},
    {"install",     gcscope_probe_install,     METH_NOARGS, "append on_gc to gc.callbacks"},
    {"region_addr", gcscope_probe_region_addr, METH_NOARGS, "address of this interpreter's Ring region"},
    {"geometry",    gcscope_probe_geometry,    METH_NOARGS, "region geometry, for the reader"},
    {"records",     gcscope_probe_records,     METH_NOARGS, "count of Records published"},
    {NULL, NULL, 0, NULL}
};

static struct PyModuleDef gcscope_probe_module = {
    PyModuleDef_HEAD_INIT, "gcscope_probe",
    "3.15-shaped GC statistics Ring for CPython 3.14", -1, gcscope_probe_methods,
    NULL, NULL, NULL, NULL
};

PyMODINIT_FUNC
PyInit_gcscope_probe(void)
{
    /* Static asserts: if any of these fire, gcscope's decoder would misread the region. */
    Py_BUILD_ASSERT(sizeof(struct gcscope_probe_generation_stats) == 64);
    Py_BUILD_ASSERT(offsetof(struct gcscope_probe_generation_stats, ts_start) == 0);
    Py_BUILD_ASSERT(offsetof(struct gcscope_probe_generation_stats, ts_stop) == 8);
    Py_BUILD_ASSERT(offsetof(struct gcscope_probe_generation_stats, collections) == 16);
    Py_BUILD_ASSERT(offsetof(struct gcscope_probe_generation_stats, collected) == 24);
    Py_BUILD_ASSERT(offsetof(struct gcscope_probe_generation_stats, uncollectable) == 32);
    Py_BUILD_ASSERT(offsetof(struct gcscope_probe_generation_stats, candidates) == 40);
    Py_BUILD_ASSERT(offsetof(struct gcscope_probe_generation_stats, duration) == 48);
    Py_BUILD_ASSERT(offsetof(struct gcscope_probe_generation_stats, heap_size) == 56);
    /* gcscope's compute_ring_base_offsets: bases[1] = young*item + 8, bases[2] =
     * bases[1] + old*item + 8. Stated as struct sizes + the base of `old` -- offsetof with an
     * array subscript is not a constant expression under MSVC's C compiler. Written in terms
     * of the depth macros so a rebuilt Ring re-checks itself. */
#define GCSCOPE_PROBE_ITEM (sizeof(struct gcscope_probe_generation_stats))
#define GCSCOPE_PROBE_YOUNG_BYTES (GC_YOUNG_STATS_SIZE * GCSCOPE_PROBE_ITEM + 8)
#define GCSCOPE_PROBE_OLD_BYTES   (GC_OLD_STATS_SIZE * GCSCOPE_PROBE_ITEM + 8)
    Py_BUILD_ASSERT(GC_YOUNG_STATS_SIZE >= 1 && GC_OLD_STATS_SIZE >= 1);
    /* `claimed` is a field an out-of-process reader inspects at the offset the header
     * publishes, so it has to be a plain int in memory. An implementation storing a lock
     * beside the value would keep every offsetof here correct and hand that reader a different
     * field. Size is the property that matters. ATOMIC_INT_LOCK_FREE is not: MSVC reports a
     * lower value there than gcc for a type it lays out the same way. */
    Py_BUILD_ASSERT(sizeof(atomic_int) == sizeof(int));
    Py_BUILD_ASSERT(sizeof(struct gcscope_probe_young_stats_buffer)
                    == GCSCOPE_PROBE_YOUNG_BYTES);
    Py_BUILD_ASSERT(sizeof(struct gcscope_probe_old_stats_buffer)
                    == GCSCOPE_PROBE_OLD_BYTES);
    Py_BUILD_ASSERT(offsetof(struct gcscope_probe_stats, old) == GCSCOPE_PROBE_YOUNG_BYTES);
    Py_BUILD_ASSERT(sizeof(struct gcscope_probe_stats)
                    == GCSCOPE_PROBE_YOUNG_BYTES + 2 * GCSCOPE_PROBE_OLD_BYTES);

    /* Version gate. The transcribed offsets were verified against 3.14.0/3.14.4/3.14.5 only;
     * on any other minor version they would read whatever happens to sit at those addresses.
     * Refuse at import rather than publish plausible garbage.
     *
     * Py_Version is the RUNTIME version (pylifecycle.h:64), not PY_VERSION_HEX, which is what
     * this was compiled against. They differ exactly in the case that matters: one module
     * built against 3.14.5 loads fine into any other 3.14.x, since the ABI is stable across a
     * minor version. */
    unsigned long ver = Py_Version;
    unsigned int major = (unsigned int)((ver >> 24) & 0xFF);
    unsigned int minor = (unsigned int)((ver >> 16) & 0xFF);
    unsigned int micro = (unsigned int)((ver >> 8) & 0xFF);
    if (major != 3 || minor != 14) {
        PyErr_Format(PyExc_ImportError,
                     "gcscope_probe supports CPython 3.14.x only; this interpreter is "
                     "%u.%u.%u, whose internal offsets this build has not verified",
                     major, minor, micro);
        return NULL;
    }

    /* Publish the fields that cannot be static initializers. Done before the module object
     * exists, so a reader that finds the header can always trust them. */
    gcscope_probe_header.header_size = (uint32_t)sizeof(struct gcscope_probe_header_t);
    gcscope_probe_header.slots_addr  = (uint64_t)(uintptr_t)&gcscope_probe_slots[0];
    gcscope_probe_header.py_version  = (uint32_t)ver;
    gcscope_probe_header.collector   = (micro >= 5) ? GCSCOPE_PROBE_COLLECTOR_GENERATIONAL
                                                    : GCSCOPE_PROBE_COLLECTOR_INCREMENTAL;

    return PyModule_Create(&gcscope_probe_module);
}
