/* The interpreter offsets the Probe reads `heap_size` through, as compile-time facts of the
 * interpreter this module was built against (ADR 0013).
 *
 * `internals.c` computes these with `offsetof` against CPython's own internal headers. This
 * header names no CPython internal type on purpose: the main translation unit compiles WITHOUT
 * `Py_BUILD_CORE`, and including those headers there would flip `PyAPI_DATA` from dllimport to
 * dllexport and break linking against python3xx.lib.
 *
 * Variables rather than macros, because a build cannot know the numbers. Reading `heap_size`
 * costs one extra load inside a gc callback that already runs a few hundred ns.
 */
#ifndef GCSCOPE_PROBE_INTERNALS_H
#define GCSCOPE_PROBE_INTERNALS_H

/* The version floor, stated where both translation units see it: each includes this after
 * Python.h, so whichever the toolchain reaches first stops with this rather than with a cascade.
 *
 * `PyInit_gcscope_probe`'s gate cannot cover 3.12, because a module has to build before it can
 * refuse anything, and there will never be a wheel below 3.13 for one to arrive prebuilt.
 * `PyTime_t` and `PyTime_PerfCounterRaw` became public in 3.13, so a 3.12 build otherwise ends
 * on six undeclared-identifier errors a page into the callback, naming none of this.
 *
 * No matching ceiling: a 3.15 build compiles and refuses at import, which says more than a
 * compiler diagnostic can, and pyproject's requires-python stops pip before either.
 *
 * `patchlevel.h` rather than `Python.h`, which this header must not pull in: `internals.c`
 * defines `Py_BUILD_CORE` before including it, and a translation unit that reached Python.h
 * through here first would get the non-core spelling and no warning about it. */
#include <patchlevel.h>
#if PY_VERSION_HEX < 0x030D0000
#  error "gcscope_probe supports CPython 3.13 and 3.14. Below 3.13 there is no public \
PyTime_PerfCounterRaw to time a Collection with, so a Probe there would hand-roll a clock per \
platform and have to prove it matches CPython's own."
#endif

#include <stddef.h>

/* offsetof(PyInterpreterState, gc): 7400 on Windows x64, 7408 on Linux x86-64, same 3.14.
 * Transcribing one of those and shipping it to the other is what this replaces. */
extern const size_t gcscope_probe_interp_gc_off;

/* Within `struct _gc_runtime_state`. */
extern const size_t gcscope_probe_gc_heap_size_off;
extern const size_t gcscope_probe_gc_collecting_off;

/* Whether `_gc_runtime_state` has a `heap_size` field at all: 1 on 3.14, 0 on 3.13, where the
 * field arrived with the collector rework and does not exist. Absent is not zero, and a read
 * at the offset anyway would return the top of the struct.
 *
 * `gcscope_probe_gc_heap_size_off` means nothing when this is 0, so check this first. Ticket 06
 * of `specs/0013-probe-portable-core.md` carries the absence out to a reader, which cannot tell
 * it from a failed self-check today. */
extern const int gcscope_probe_has_heap_size;

#endif /* GCSCOPE_PROBE_INTERNALS_H */
