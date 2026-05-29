from setuptools import setup, find_packages

setup(
    name="ams-memory-hermes",
    version="2.0.0",
    description="Asuna Memory System - Hermes Provider",
    author="AMS Team",
    packages=find_packages(),
    install_requires=[
        "aiohttp>=3.9.0",
        "pyyaml>=6.0",
        "requests>=2.31.0",
    ],
    python_requires=">=3.8",
    entry_points={
        "hermes.providers": [
            "ams_memory = ams_memory.provider:AMSProvider",
        ],
    },
)
