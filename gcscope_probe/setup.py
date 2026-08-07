"""Declares the one extension module. Metadata lives in pyproject.toml.

The module name fixes the built filename's prefix, and gcscope matches on that prefix to
discover a Probe (ADR 0014). Rename it and discovery breaks with no error anywhere; see the
header declaration in `src/gcscope_probe.c`.

No compiler, SDK or interpreter path appears here or elsewhere in the repository. setuptools
locates the toolchain, so `pip install .` is the build.
"""

from setuptools import Extension, setup
from setuptools.command.build_ext import build_ext


class BuildExt(build_ext):
    """Give MSVC the two flags it needs for `<stdatomic.h>`.

    MSVC compiles `.c` in a legacy dialect unless told otherwise, and gates C11 atomics behind
    a second opt-in. Without them `vcruntime_c11_stdatomic.h` stops the build: `"C atomics
    require C11 or later"` for the missing `/std:c11`, then `"C atomic support is not enabled"`
    for the missing `/experimental:c11atomics`. Despite its name the second flag is how VS 2022
    ships C11 atomics, from 17.5 on, which puts a floor under the Windows toolchain.

    gcc and clang default to a gnu1x mode that has the header, so pinning a standard there
    would cap the dialect Python.h compiles under and buy nothing.
    """

    MSVC_C11_ATOMICS = ["/std:c11", "/experimental:c11atomics"]

    def build_extensions(self):
        if self.compiler.compiler_type == "msvc":
            for ext in self.extensions:
                ext.extra_compile_args = [*ext.extra_compile_args, *self.MSVC_C11_ATOMICS]
        super().build_extensions()


setup(
    cmdclass={"build_ext": BuildExt},
    ext_modules=[
        Extension(
            name="gcscope_probe",
            sources=["src/gcscope_probe.c"],
        )
    ],
)
