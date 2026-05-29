from setuptools import setup, find_packages

setup(
    name="ams-memory",
    version="2.0.0",
    description="Asuna Memory System - Hermes Agent Memory Provider",
    packages=find_packages(),
    install_requires=[
        "requests>=2.28.0",
    ],
    python_requires=">=3.8",
    # Note: Hermes discovers plugins via file-scanning $HERMES_HOME/plugins/,
    # not via entry_points. This file is for standalone pip install only.
)
