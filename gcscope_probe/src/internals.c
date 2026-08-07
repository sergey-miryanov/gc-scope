/* The only translation unit compiled WITH `Py_BUILD_CORE`.
 *
 * `_gc_runtime_state` is internal and has no accessor, so the Probe reaches `heap_size` and
 * `collecting` through byte offsets. ADR 0013 says where those offsets come from: CPython's own
 * internal headers, at compile time, for the exact interpreter this wheel targets. A wheel is
 * built per minor per platform ABI, so those headers describe precisely the build the module
 * will load into -- better information than a runtime registry can give, which is why the Probe
 * does not reuse gcscope's.
 *
 * This file references no Python *data* symbol, and that is what lets it exist. `Py_BUILD_CORE`
 * flips `PyAPI_DATA` from dllimport to dllexport; touching `Py_None` here would emit a
 * definition MSVC then refuses to reconcile with python3xx.lib. Adding anything to this file
 * that dereferences an interpreter object will fail at link time on Windows and nowhere else.
 *
 * Nothing runs at build time. The alternative -- generate a header by compiling and executing a
 * program, which the prototype's `.bat` files did -- cannot survive cross-compilation, and the
 * macOS arm64 and emulated Linux wheels of `specs/0015-publish-probe-wheels.md` are exactly
 * that case.
 */
#define Py_BUILD_CORE 1
#define PY_SSIZE_T_CLEAN
#include <Python.h>

/* 3.14 consolidated `PyInterpreterState` and `_gc_runtime_state` into this one header; on 3.13
 * they sit in `pycore_interp.h` and `pycore_gc.h`. `specs/0013-probe-portable-core.md` §4 adds
 * that branch with 3.13 support in ticket 05, keyed on `PY_VERSION_HEX`.
 *
 * The guard turns a missing internal-headers install into an instruction. Without it the build
 * stops at "cannot open include file", naming a header the person building has no reason to
 * recognise and no obvious way to get.
 *
 * Nested rather than `#if defined(__has_include) && !__has_include(...)`. `&&` does not
 * short-circuit at parse time, so a preprocessor without `__has_include` would substitute 0 and
 * then have to parse `0(<internal/pycore_interp_structs.h>)`, which is a syntax error rather
 * than the fallback intended. Every compiler in range defines it; the nested form costs nothing
 * to be sure.
 *
 * The include below stays unconditional. `#error` is fatal to MSVC and not to gcc, so on gcc
 * the message is followed by the incomplete-type errors it was meant to explain. Putting the
 * include in an `#else` would suppress those and leave the rest of the file referring to types
 * that do not exist, which trades one cascade for another. First diagnostic wins. */
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

/* The offsets alone do not say what sits there. `gcscope_probe.c` reads one field as
 * `Py_ssize_t` and the other as `int`, and CPython retyping either would leave every offset
 * above correct and both reads wrong -- `collecting` narrowing to `int8_t` would hand the
 * self-check three bytes of whatever follows. This is the check the transcribed constants could
 * not have.
 *
 * `_Static_assert` rather than `Py_BUILD_ASSERT`, which expands to a statement and needs a
 * function to sit in. This file has none, deliberately. */
_Static_assert(sizeof(((struct _gc_runtime_state *)0)->heap_size) == sizeof(Py_ssize_t),
               "_gc_runtime_state.heap_size is no longer a Py_ssize_t");
_Static_assert(sizeof(((struct _gc_runtime_state *)0)->collecting) == sizeof(int),
               "_gc_runtime_state.collecting is no longer an int");
