"""Declares the one extension module. All metadata lives in pyproject.toml.

The module name is `gcscope_probe`, which fixes the built filename's prefix — and that
prefix is what gcscope matches on to discover a Probe (ADR 0014). Changing it here breaks
discovery in every released gcscope, silently; see the comment at the header declaration in
`src/gcscope_probe.c`.

No compiler paths, SDK paths or interpreter paths appear anywhere in this file or in the
repository. setuptools locates the toolchain itself, which is the whole point of replacing
the prototype's three `.bat` files: `pip install .` is the build.
"""

from setuptools import Extension, setup

setup(
    ext_modules=[
        Extension(
            name="gcscope_probe",
            sources=["src/gcscope_probe.c"],
        )
    ],
)
