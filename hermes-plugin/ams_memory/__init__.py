"""AMS Memory Provider for Hermes"""

from .provider import AMSMemoryProvider, register

__version__ = "3.0.0"  # kept in sync with the asuna-memory crate (enforced by tests/doc_truth.rs)
__all__ = ["AMSMemoryProvider", "register"]
