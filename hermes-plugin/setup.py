from setuptools import setup, find_packages

setup(
    name="ams-memory",
    # Kept in sync with Cargo.toml; enforced automatically by tests/doc_truth.rs.
    version="3.0.0",
    description="Asuna Memory System - Hermes Agent Memory Provider",
    packages=find_packages(include=["ams_memory", "ams_memory.*"]),
    install_requires=[
        "requests>=2.32.4",  # CVE-2024-47081 (.netrc leak); requirements.txt hash-pinned to 2.32.4
    ],
    python_requires=">=3.9",  # N-B: 锁定 urllib3==2.8.0 需 >=3.9（urllib3 自 2.3 起放弃 3.8）
    # Note: Hermes discovers plugins via file-scanning $HERMES_HOME/plugins/,
    # not via entry_points. This file is for standalone pip install only.
)
