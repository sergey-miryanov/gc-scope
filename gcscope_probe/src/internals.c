/* The only translation unit compiled WITH `Py_BUILD_CORE`.
 *
 * `_gc_runtime_state` is internal and has no accessor, so the Probe reaches `heap_size` and
 * `collecting` through byte offsets. ADR 0013 takes those offsets from CPython's own internal
 * headers at compile time. A wheel is built per minor per platform ABI, so those headers
 * describe the exact build the module will load into, which is why the Probe does not reuse
 * gcscope's runtime registry.
 *
 * This file exists only because it references no Python *data* symbol. `Py_BUILD_CORE` flips
 * `PyAPI_DATA` from dllimport to dllexport; touching `Py_None` would emit a definition MSVC
 * then refuses to reconcile with python3xx.lib. Anything added below that dereferences an
 * interpreter object fails at link time on Windows and nowhere else.
 *
 * Nothing runs at build time either. Generating a header by compiling and executing a program,
 * as the prototype's `.bat` files did, cannot survive the cross-compilation that
 * `specs/0015-publish-probe-wheels.md` needs for macOS arm64 and emulated Linux.
 */
#define Py_BUILD_CORE 1
#define PY_SSIZE_T_CLEAN
#include <Python.h>

/* 3.14 consolidated `PyInterpreterState` and `_gc_runtime_state` into this one header; 3.13
 * splits them across `pycore_interp.h` and `pycore_gc.h`. Ticket 05 of
 * `specs/0013-probe-portable-core.md` §4 adds that branch, keyed on `PY_VERSION_HEX`.
 *
 * The guard turns a missing internal-headers install into an instruction. Without it the build
 * stops at "cannot open include file", naming a header the person building has no reason to
 * recognise.
 *
 * Nested rather than `#if defined(__has_include) && !__has_include(...)`, because `&&` does not
 * short-circuit at parse time: a preprocessor without `__has_include` substitutes 0 and then
 * has to parse `0(<internal/pycore_interp_structs.h>)`, a syntax error rather than the intended
 * fallback.
 *
 * The include below stays unconditional. `#error` is fatal to MSVC and not to gcc, so gcc
 * prints the message and then the incomplete-type errors it explains. Moving the include into
 * an `#else` would leave the rest of the file referring to types that do not exist, trading one
 * cascade for another. First diagnostic wins. */
#ifdef __has_include
#  if !__has_include(<internal/pycore_interp_structs.h>)
#    error "gcscope_probe needs CPython's internal headers: <internal/pycore_interp_structs.h> \
is missing from this interpreter's include directory. They ship with the python.org installers \
and in pythonX.Y-dev on Debian and Ubuntu. Install the development headers for the interpreter \
you are building against, then rebuild."
#  endif
#endif
#include <internal/pycore_interp_structs.h>

#include "internals.h"

const size_t gcscope_probe_interp_gc_off = offsetof(PyInterpreterState, gc);
const size_t gcscope_probe_gc_heap_size_off = offsetof(struct _gc_runtime_state, heap_size);
const size_t gcscope_probe_gc_collecting_off = offsetof(struct _gc_runtime_state, collecting);

/* The offsets do not say what sits at them. `gcscope_probe.c` reads one field as `Py_ssize_t`
 * and the other as `int`; CPython retyping either leaves every offset above correct and both
 * reads wrong, and `collecting` narrowing to `int8_t` would hand the self-check three bytes of
 * whatever follows. The transcribed constants could not express this check.
 *
 * `_Static_assert` rather than `Py_BUILD_ASSERT`, which expands to a statement and needs a
 * function to sit in. This file has none, deliberately. */
_Static_assert(sizeof(((struct _gc_runtime_state *)0)->heap_size) == sizeof(Py_ssize_t),
               "_gc_runtime_state.heap_size is no longer a Py_ssize_t");
_Static_assert(sizeof(((struct _gc_runtime_state *)0)->collecting) == sizeof(int),
               "_gc_runtime_state.collecting is no longer an int");
