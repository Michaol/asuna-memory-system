"""
AMS Memory Provider for Hermes
Integrates Asuna Memory System with Hermes Agent
"""

import asyncio
import logging
import uuid
from typing import Any, Dict, List
from datetime import datetime, timezone

try:
    import aiohttp
except ImportError:
    aiohttp = None

from hermes.providers.base import BaseProvider
from hermes.models.message import Message

logger = logging.getLogger(__name__)


class AMSProvider(BaseProvider):
    """
    Hermes provider that integrates with AMS Gateway for multi-layer memory.

    Features:
    - L0-L5 hierarchical memory storage
    - Automatic memory recall before responses
    - Automatic memory storage after conversations
    - Evolution chain for memory versioning
    - Progressive disclosure retrieval
    """

    def __init__(self, config: Dict[str, Any]):
        super().__init__(config)
        self.gateway_url = config.get("gateway_url", "http://127.0.0.1:8765")
        self.auto_recall = config.get("auto_recall", True)
        self.auto_store = config.get("auto_store", True)
        self.recall_top_k = config.get("recall_top_k", 5)
        self.store_threshold = config.get("store_threshold", 0.7)
        self.session = None
        self._session_id = None  # Tracks current conversation session_id for capture

    async def initialize(self):
        """Initialize HTTP session for Gateway communication"""
        if aiohttp is None:
            logger.error("aiohttp not installed. Install with: pip install aiohttp")
            return

        self.session = aiohttp.ClientSession()
        logger.info(f"AMS Provider initialized, Gateway: {self.gateway_url}")

    async def cleanup(self):
        """Cleanup HTTP session"""
        if self.session:
            await self.session.close()
            self.session = None

    async def before_response(self, messages: List[Message]) -> List[Message]:
        """
        Hook called before generating response.
        Recalls relevant memories and injects them into context.
        """
        if not self.auto_recall or not self.session:
            return messages

        # Extract query from last user message
        user_messages = [m for m in messages if m.role == "user"]
        if not user_messages:
            return messages

        query = user_messages[-1].content

        try:
            # Call AMS recall endpoint
            async with self.session.post(
                f"{self.gateway_url}/recall",
                json={
                    "query": query,
                    "top_k": self.recall_top_k,
                },
                timeout=aiohttp.ClientTimeout(total=5.0),
            ) as resp:
                if resp.status == 200:
                    data = await resp.json()
                    memories = data.get("memories", [])

                    if memories:
                        # Format memories as system message
                        memory_context = self._format_memories(memories)
                        system_msg = Message(
                            role="system",
                            content=memory_context,
                        )
                        # Insert after first system message
                        messages = self._insert_after_system(messages, system_msg)
                        logger.info(f"Recalled {len(memories)} memories")

        except asyncio.TimeoutError:
            logger.warning("AMS recall timeout")
        except Exception as e:
            logger.exception("AMS recall failed: %s", e)

        return messages

    async def after_response(self, messages: List[Message], response: Message):
        """
        Hook called after generating response.
        Stores conversation as memories if appropriate.
        """
        if not self.auto_store or not self.session:
            return

        try:
            # Generate or reuse session_id for this conversation
            if self._session_id is None:
                self._session_id = str(uuid.uuid4())

            # Extract conversation turns with Unix ms timestamps (matches CaptureRequest schema)
            now_ms = int(datetime.now(timezone.utc).timestamp() * 1000)
            conversation = []
            for msg in messages[-10:]:  # Last 10 messages
                conversation.append({
                    "role": msg.role,
                    "content": msg.content,
                    "timestamp": now_ms,
                })

            # Add response
            conversation.append({
                "role": response.role,
                "content": response.content,
                "timestamp": now_ms,
            })

            # Call AMS capture endpoint (matches CaptureRequest: session_id + turns)
            async with self.session.post(
                f"{self.gateway_url}/capture",
                json={
                    "session_id": self._session_id,
                    "turns": conversation,
                },
                timeout=aiohttp.ClientTimeout(total=10.0),
            ) as resp:
                if resp.status == 200:
                    data = await resp.json()
                    turns_saved = data.get("turns_saved", 0)
                    if turns_saved > 0:
                        logger.info(f"Captured {turns_saved} turns (session: {self._session_id})")

        except asyncio.TimeoutError:
            logger.warning("AMS capture timeout")
        except Exception as e:
            logger.exception("AMS capture failed: %s", e)

    def _format_memories(self, memories: List[Dict[str, Any]]) -> str:
        """Format recalled memories as context for LLM"""
        if not memories:
            return ""

        lines = ["<recalled_memories>"]

        for i, mem in enumerate(memories, 1):
            # Basic info
            lines.append(f"\n[Memory {i}]")
            lines.append(f"Content: {mem.get('content', 'N/A')}")
            lines.append(f"Type: {mem.get('memory_type', 'unknown')}")
            lines.append(f"Confidence: {mem.get('confidence_score', 0):.2f}")

            # Evolution chain (if present)
            evolution = mem.get("evolution_chain", [])
            if evolution and len(evolution) > 1:
                lines.append(f"Evolution: {len(evolution)} versions")

            # Related memories
            related = mem.get("related_memories", [])
            if related:
                lines.append(f"Related: {len(related)} memories")

        lines.append("\n</recalled_memories>")
        lines.append("\nUse these memories to inform your response, but don't explicitly mention them unless relevant.")

        return "\n".join(lines)

    def _insert_after_system(self, messages: List[Message], new_msg: Message) -> List[Message]:
        """Insert a message after the first system message"""
        result = []
        inserted = False

        for msg in messages:
            result.append(msg)
            if msg.role == "system" and not inserted:
                result.append(new_msg)
                inserted = True

        # If no system message, prepend
        if not inserted:
            result = [new_msg] + result

        return result

    # Provider interface methods

    async def get_completion(self, messages: List[Message], **kwargs) -> Message:
        """
        Generate completion using wrapped provider with memory augmentation.
        This method should be overridden by the actual LLM provider.

        AMSProvider is designed as a wrapper — compose it with a real LLM provider
        (e.g., OpenAIProvider, AnthropicProvider) that implements get_completion.
        """
        raise NotImplementedError(
            "AMSProvider is a memory wrapper and does not generate completions itself. "
            "Compose it with a concrete LLM provider (e.g., OpenAIProvider) that implements get_completion."
        )

    async def stream_completion(self, messages: List[Message], **kwargs):
        """Stream completion - delegate to wrapped provider"""
        raise NotImplementedError(
            "AMSProvider is a memory wrapper and does not stream completions itself. "
            "Compose it with a concrete LLM provider that implements stream_completion."
        )


# Export for plugin discovery
__all__ = ["AMSProvider"]
