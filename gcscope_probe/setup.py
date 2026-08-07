"""Declares the one extension module. Metadata lives in pyproject.toml.

The module name fixes the built filename's prefix, and gcscope matches on that prefix to
discover a Probe (ADR 0014). Rename it and discovery breaks with no error anywhere; see the
header declaration in `src/gcscope_probe.c`.

No compiler, SDK or interpreter path appears here or elsewhere in the repository. setuptools
locates the toolchain, so `pip install .` is the build.
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
