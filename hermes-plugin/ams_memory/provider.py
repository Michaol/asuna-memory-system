"""
AMS Memory Provider for Hermes — implements the MemoryProvider ABC.

Integrates Asuna Memory System Gateway (HTTP REST) as a memory backend
for Hermes Agent. Uses synchronous HTTP (requests) since all ABC methods
are synchronous.

Official ABC: agent/memory_provider.py in NousResearch/hermes-agent
4 abstract methods: name, is_available, initialize, get_tool_schemas
All methods synchronous.

Gateway endpoints used:
  GET  /health       — availability check
  POST /recall       — progressive disclosure retrieval (L3→L2→L1→L0)
  POST /capture      — save conversation turns
  POST /session/end  — session end signal
"""

import json
import logging
import uuid
from abc import ABC
from datetime import datetime, timezone
from typing import Any, Dict, List, Optional

try:
    from agent.memory_provider import MemoryProvider  # type: ignore[import-not-found]
except ImportError:
    # Development without Hermes installed — fall back to ABC
    MemoryProvider = ABC  # type: ignore[assignment,misc]

try:
    import requests
except ImportError:
    requests = None

logger = logging.getLogger(__name__)

# Timeouts (seconds)
_RECALL_TIMEOUT = 5
_CAPTURE_TIMEOUT = 10
_HEALTH_TIMEOUT = 2


class AMSMemoryProvider(MemoryProvider):
    """
    Hermes MemoryProvider implementation backed by AMS Gateway.

    Lifecycle (called by Hermes MemoryManager):
      1. register(ctx)            — module-level registration
      2. initialize(session_id)   — called once at session start
      3. system_prompt_block()    — static prompt injected once
      4. prefetch(query)          — called before each LLM API call
      5. sync_turn(user, asst)    — called after each turn completes
      6. handle_tool_call(name, args) — called when LLM invokes a memory tool
      7. on_session_end(messages) — optional cleanup at session exit
      8. shutdown()               — clean shutdown, flush/close connections

    All gateway calls are best-effort: failures are logged but never
    raise, so memory issues don't break the agent loop.
    """

    def __init__(self, config: Optional[Dict[str, Any]] = None):
        cfg = config or {}
        self.gateway_url = cfg.get("gateway_url", "http://127.0.0.1:8765").rstrip("/")
        self.api_key = cfg.get("api_key", "")
        self.recall_top_k = cfg.get("recall_top_k", 5)
        self.auto_recall = cfg.get("auto_recall", True)
        self.auto_store = cfg.get("auto_store", True)
        self._session_id: str = ""
        self._hermes_home: str = ""
        self._platform: str = ""

    # ── Abstract method implementations ─────────────────────────

    @property
    def name(self) -> str:
        return "ams_memory"

    def is_available(self) -> bool:
        """Check if requests library is installed and gateway_url is set."""
        if requests is None:
            return False
        return bool(self.gateway_url)

    def initialize(self, session_id: str, **kwargs) -> None:
        """
        Initialize for a session.

        kwargs always include:
          - hermes_home (str): active HERMES_HOME directory path
          - platform (str): "cli", "telegram", "discord", "cron", etc.
        kwargs may include:
          - agent_context, agent_identity, agent_workspace,
            parent_session_id, user_id, user_id_alt
        """
        self._session_id = session_id or str(uuid.uuid4())
        self._hermes_home = kwargs.get("hermes_home", "")
        self._platform = kwargs.get("platform", "")
        logger.info(
            "AMS MemoryProvider initialized (gateway=%s, session=%s, platform=%s)",
            self.gateway_url,
            self._session_id,
            self._platform,
        )

    def get_tool_schemas(self) -> List[Dict[str, Any]]:
        """
        Return tool schemas to expose AMS memory tools to the LLM.

        These allow the model to explicitly search or write memories
        via tool calls, in addition to the automatic prefetch/sync.
        """
        return [
            {
                "name": "memory_search",
                "description": "Search persistent memory for relevant information from past conversations.",
                "parameters": {
                    "type": "object",
                    "required": ["query"],
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query for memory retrieval",
                        },
                        "top_k": {
                            "type": "integer",
                            "description": "Max results (default 5)",
                            "default": 5,
                        },
                    },
                },
            },
            {
                "name": "memory_save",
                "description": "Save an important fact or observation to persistent memory.",
                "parameters": {
                    "type": "object",
                    "required": ["content"],
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "The fact or observation to remember",
                        },
                        "confidence": {
                            "type": "string",
                            "enum": ["high", "medium", "low"],
                            "default": "medium",
                        },
                    },
                },
            },
        ]

    # ── Optional overrides (with defaults in ABC) ───────────────

    def system_prompt_block(self) -> str:
        """
        Static text injected into the system prompt once.
        Dynamic per-query recall happens in prefetch() instead.
        """
        return (
            "You have access to a persistent memory system. "
            "Relevant memories from past conversations will be provided "
            "before each user message. Use them to inform your responses "
            "but do not explicitly reference them unless the user asks."
        )

    def prefetch(self, query: str, *, session_id: str = "") -> str:  # noqa: ARG002
        """
        Recall relevant memories before each LLM API call.

        Called by Hermes before every API call with the user's query.
        Returns formatted text to inject, or "" if nothing relevant.
        Must be fast.
        """
        if not self.auto_recall:
            return ""
        if not query or not query.strip():
            return ""

        try:
            resp = requests.post(
                f"{self.gateway_url}/recall",
                json={"query": query, "top_k": self.recall_top_k},
                timeout=_RECALL_TIMEOUT,
                headers=self._auth_headers(),
            )
            if resp.status_code != 200:
                logger.debug("AMS recall returned %d", resp.status_code)
                return ""

            data = resp.json()
            memories = data.get("memories", [])
            if not memories:
                return ""

            logger.debug("AMS recalled %d memories for query", len(memories))
            return self._format_memories(memories)

        except requests.exceptions.Timeout:
            logger.warning("AMS recall timeout")
            return ""
        except Exception as e:
            logger.warning("AMS recall failed: %s", e)
            return ""

    def sync_turn(
        self,
        user_content: str,
        assistant_content: str,
        *,
        session_id: str = "",
        messages: Optional[List[Dict[str, Any]]] = None,
    ) -> None:
        """
        Persist a completed turn. Should be non-blocking.

        Called by Hermes after each turn with the user message and
        assistant response. messages contains the full OpenAI-style
        conversation list including tool calls/results.
        """
        if not self.auto_store:
            return
        if not user_content and not assistant_content:
            return

        sid = session_id or self._session_id
        if not sid:
            sid = str(uuid.uuid4())
            self._session_id = sid

        now_ms = int(datetime.now(timezone.utc).timestamp() * 1000)

        turns = []
        if user_content:
            turns.append({
                "role": "user",
                "content": user_content,
                "timestamp": now_ms,
            })
        if assistant_content:
            turns.append({
                "role": "assistant",
                "content": assistant_content,
                "timestamp": now_ms,
            })

        try:
            resp = requests.post(
                f"{self.gateway_url}/capture",
                json={"session_id": sid, "turns": turns},
                timeout=_CAPTURE_TIMEOUT,
                headers=self._auth_headers(),
            )
            if resp.status_code == 200:
                saved = resp.json().get("turns_saved", 0)
                logger.debug("AMS captured %d turns (session=%s)", saved, sid)
            else:
                logger.debug("AMS capture returned %d", resp.status_code)

        except requests.exceptions.Timeout:
            logger.warning("AMS capture timeout")
        except Exception as e:
            logger.warning("AMS capture failed: %s", e)

    def handle_tool_call(self, tool_name: str, args: Dict[str, Any], **kwargs) -> str:
        """
        Handle a tool call. Must return a JSON string.
        Only called for tool names returned by get_tool_schemas().
        """
        if tool_name == "memory_search":
            result = self._tool_search(args)
        elif tool_name == "memory_save":
            result = self._tool_save(args)
        else:
            raise NotImplementedError(f"Provider {self.name} does not handle tool {tool_name}")
        return json.dumps(result)

    def on_turn_start(self, turn_number: int, message: str, **kwargs) -> None:  # noqa: ARG002
        """Start of each turn. Optional — no action needed for AMS."""

    def on_session_end(self, messages: List[Dict[str, Any]]) -> None:  # noqa: ARG002
        """
        Session exit/timeout. Sends session_end signal to the gateway
        for future aggregation pipeline use.
        """
        if not self._session_id:
            return

        try:
            resp = requests.post(
                f"{self.gateway_url}/session/end",
                json={"session_id": self._session_id},
                timeout=_CAPTURE_TIMEOUT,
                headers=self._auth_headers(),
            )
            if resp.status_code == 200:
                logger.info("AMS session ended: %s", self._session_id)
        except Exception as e:
            logger.debug("AMS session_end failed: %s", e)

    def on_session_switch(
        self,
        new_session_id: str,
        *,
        parent_session_id: str = "",  # noqa: ARG002
        reset: bool = False,
        **kwargs,  # noqa: ARG002
    ) -> None:
        """Handle session switch (/resume, /branch, /reset, /new)."""
        self._session_id = new_session_id
        logger.info("AMS session switched to %s (reset=%s)", new_session_id, reset)

    def shutdown(self) -> None:
        """Clean shutdown — no persistent connections to close."""
        logger.debug("AMS MemoryProvider shutdown")

    # ── Private helpers ─────────────────────────────────────────

    def _auth_headers(self) -> Dict[str, str]:
        """Build auth headers if API key is configured."""
        if self.api_key:
            return {"Authorization": f"Bearer {self.api_key}"}
        return {}

    def _format_memories(self, memories: List[Dict[str, Any]]) -> str:
        """Format recalled memories as a context block for the LLM."""
        if not memories:
            return ""

        lines = ["<recalled_memories>"]
        for i, mem in enumerate(memories, 1):
            layer = mem.get("layer", "?")
            content = mem.get("content", "N/A")
            mem_type = mem.get("type", mem.get("memory_type", "unknown"))
            confidence = mem.get("confidence", mem.get("confidence_score", 0))

            lines.append(f"\n[Memory {i}] ({layer}/{mem_type}, confidence={confidence:.2f})")
            lines.append(content)

        lines.append("\n</recalled_memories>")
        return "\n".join(lines)

    def _tool_search(self, args: Dict[str, Any]) -> Dict[str, Any]:
        """Handle memory_search tool call from LLM."""
        query = args.get("query", "")
        top_k = args.get("top_k", 5)
        if not query:
            return {"error": "No query provided.", "memories": []}

        try:
            resp = requests.post(
                f"{self.gateway_url}/recall",
                json={"query": query, "top_k": top_k},
                timeout=_RECALL_TIMEOUT,
                headers=self._auth_headers(),
            )
            if resp.status_code == 200:
                data = resp.json()
                return {"memories": data.get("memories", [])}
            return {"error": f"Search failed (HTTP {resp.status_code}).", "memories": []}
        except Exception as e:
            return {"error": f"Search failed: {e}", "memories": []}

    def _tool_save(self, args: Dict[str, Any]) -> Dict[str, Any]:
        """Handle memory_save tool call from LLM."""
        content = args.get("content", "")
        confidence = args.get("confidence", "medium")
        if not content:
            return {"error": "No content provided.", "saved": False}

        if not self._session_id:
            self._session_id = str(uuid.uuid4())

        now_ms = int(datetime.now(timezone.utc).timestamp() * 1000)
        try:
            resp = requests.post(
                f"{self.gateway_url}/capture",
                json={
                    "session_id": self._session_id,
                    "turns": [{
                        "role": "system",
                        "content": f"[Memory saved] {content}",
                        "timestamp": now_ms,
                    }],
                },
                timeout=_CAPTURE_TIMEOUT,
                headers=self._auth_headers(),
            )
            if resp.status_code == 200:
                return {"saved": True, "confidence": confidence}
            return {"error": f"Save failed (HTTP {resp.status_code}).", "saved": False}
        except Exception as e:
            return {"error": f"Save failed: {e}", "saved": False}


# ── Plugin registration ─────────────────────────────────────────


def register(ctx) -> None:
    """Register AMSMemoryProvider with Hermes MemoryManager."""
    ctx.register_memory_provider(AMSMemoryProvider())


# Export for plugin discovery
__all__ = ["AMSMemoryProvider", "register"]
