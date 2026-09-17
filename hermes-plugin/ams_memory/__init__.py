"""AMS Memory Provider for Hermes"""

from .provider import AMSMemoryProvider, register

__version__ = "2.7.1"  # kept in sync with the asuna-memory crate (J47)
__all__ = ["AMSMemoryProvider", "register"]
