from setuptools import setup, find_packages

setup(
    name="ams-memory",
    # J47: kept in sync with the crate version in Cargo.toml (release
    # convention: bump Cargo.toml / Cargo.lock / here together in the same
    # commit — no automated check exists, verify by hand at release time).
    version="2.7.3",
    description="Asuna Memory System - Hermes Agent Memory Provider",
    packages=find_packages(),
    install_requires=[
        "requests>=2.28.0",
    ],
    python_requires=">=3.8",
    # Note: Hermes discovers plugins via file-scanning $HERMES_HOME/plugins/,
    # not via entry_points. This file is for standalone pip install only.
)
