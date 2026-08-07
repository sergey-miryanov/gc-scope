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

#include <stddef.h>

/* offsetof(PyInterpreterState, gc): 7400 on Windows x64, 7408 on Linux x86-64, same 3.14.
 * Transcribing one of those and shipping it to the other is what this replaces. */
extern const size_t gcscope_probe_interp_gc_off;

/* Within `struct _gc_runtime_state`. */
extern const size_t gcscope_probe_gc_heap_size_off;
extern const size_t gcscope_probe_gc_collecting_off;

#endif /* GCSCOPE_PROBE_INTERNALS_H */
