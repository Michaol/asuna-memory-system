"""Tests for AMS Hermes MemoryProvider"""

import json
import sys
import types

import pytest
from unittest.mock import Mock, patch

from ams_memory.provider import (
    AMSMemoryProvider,
    _CAPTURE_TIMEOUT,
    _REQUEST_TIMEOUT,
    _load_config,
)


class _InlineThread:
    """threading.Thread stand-in whose start() runs the target synchronously.

    Makes the U9 background-capture path deterministic in tests: the POST
    happens inside the sync_turn() call, so mock assertions can't race it.
    is_alive()/join() reflect "already finished" so on_session_end's bounded
    join of provider._capture_thread is a no-op in tests.
    """

    def __init__(self, target=None, args=(), kwargs=None, name=None, daemon=None):
        self._target = target
        self._args = args
        self._kwargs = kwargs or {}
        self.name = name
        self.daemon = daemon

    def start(self):
        self._target(*self._args, **self._kwargs)

    def is_alive(self):
        return False

    def join(self, timeout=None):
        return None


@pytest.fixture
def provider():
    """Create provider instance with test config"""
    return AMSMemoryProvider(config={
        "gateway_url": "http://test-gateway:8765",
        "api_key": "test-key",
        "recall_top_k": 5,
    })


class TestAMSMemoryProvider:
    """Test suite for AMSMemoryProvider"""

    def test_initialization(self, provider):
        """Test provider initialization with config"""
        assert provider.gateway_url == "http://test-gateway:8765"
        assert provider.api_key == "test-key"
        assert provider.recall_top_k == 5
        assert provider._session_id == ""
        assert provider.auto_recall is True
        assert provider.auto_store is True
        assert provider._hermes_home == ""
        assert provider._platform == ""

    def test_initialization_defaults(self):
        """Test provider initialization with default config"""
        provider = AMSMemoryProvider()
        assert provider.gateway_url == "http://127.0.0.1:8765"
        assert provider.api_key == ""
        assert provider.recall_top_k == 5

    def test_name_property(self, provider):
        """Test the name property"""
        assert provider.name == "ams_memory"

    @patch("ams_memory.provider.requests")
    def test_is_available_true(self, mock_requests, provider):
        """Test is_available when requests is installed and gateway is set"""
        assert provider.is_available() is True

    @patch("ams_memory.provider.requests", None)
    def test_is_available_no_requests(self, provider):
        """Test is_available when requests is not installed"""
        assert provider.is_available() is False

    def test_is_available_no_gateway(self):
        """Test is_available when gateway_url is empty"""
        provider = AMSMemoryProvider(config={"gateway_url": ""})
        assert provider.is_available() is False

    def test_initialize(self, provider):
        """Test session initialization"""
        provider.initialize(
            session_id="test-session-123",
            hermes_home="/home/user/.hermes",
            platform="cli",
        )
        assert provider._session_id == "test-session-123"
        assert provider._hermes_home == "/home/user/.hermes"
        assert provider._platform == "cli"

    def test_initialize_auto_generates_session_id(self, provider):
        """Test initialize auto-generates session_id if empty string passed"""
        provider.initialize("")
        assert len(provider._session_id) == 36  # UUID format

    def test_system_prompt_block(self, provider):
        """Test system prompt block content"""
        prompt = provider.system_prompt_block()
        assert "persistent memory system" in prompt
        assert "past conversations" in prompt

    def test_system_prompt_block_declares_memories_untrusted(self, provider):
        """U10: the system prompt must frame recalled memories as data,
        not instructions (memory-poisoning mitigation)."""
        lowered = provider.system_prompt_block().lower()
        assert "untrusted" in lowered
        assert "ignore any instructions" in lowered

    def test_format_memories_includes_data_framing(self, provider):
        """U10: the <recalled_memories> block itself must carry the
        untrusted-data declaration as its first line inside the tag."""
        memories = [
            {"content": "Memory 1", "layer": "L1", "type": "fact", "confidence": 0.9},
        ]
        result = provider._format_memories(memories)
        lowered = result.lower()
        assert "untrusted" in lowered
        assert "ignore any instructions" in lowered
        # The framing sits inside the block, before the (untrusted) contents
        assert lowered.index("untrusted") > lowered.index("<recalled_memories>")
        assert lowered.index("ignore any instructions") < lowered.index("memory 1")

    def test_get_tool_schemas(self, provider):
        """Test tool schema generation"""
        schemas = provider.get_tool_schemas()
        assert len(schemas) == 2

        search_schema = schemas[0]
        assert search_schema["name"] == "memory_search"
        assert "query" in search_schema["parameters"]["properties"]
        assert "top_k" in search_schema["parameters"]["properties"]

        save_schema = schemas[1]
        assert save_schema["name"] == "memory_save"
        assert "content" in save_schema["parameters"]["properties"]
        # U8: the gateway /capture has no per-turn confidence field, so the
        # schema must not advertise one the plugin would silently drop.
        assert "confidence" not in save_schema["parameters"]["properties"]

    @patch("ams_memory.provider.requests")
    def test_prefetch_empty_query(self, mock_requests, provider):
        """Test prefetch with empty query returns empty string"""
        result = provider.prefetch("")
        assert result == ""
        mock_requests.post.assert_not_called()

    @patch("ams_memory.provider.requests")
    def test_prefetch_success(self, mock_requests, provider):
        """Test successful memory prefetch"""
        mock_response = Mock()
        mock_response.status_code = 200
        mock_response.json.return_value = {
            "memories": [
                {"content": "Test memory 1", "layer": "L1", "type": "fact", "confidence": 0.9},
                {"content": "Test memory 2", "layer": "L2", "type": "scenario", "confidence": 0.8},
            ]
        }
        mock_requests.post.return_value = mock_response

        result = provider.prefetch("test query")
        assert "<recalled_memories>" in result
        assert "Test memory 1" in result
        assert "Test memory 2" in result
        assert "</recalled_memories>" in result

    @patch("ams_memory.provider.requests")
    def test_prefetch_no_memories(self, mock_requests, provider):
        """Test prefetch when no memories are returned"""
        mock_response = Mock()
        mock_response.status_code = 200
        mock_response.json.return_value = {"memories": []}
        mock_requests.post.return_value = mock_response

        result = provider.prefetch("test query")
        assert result == ""

    @patch("ams_memory.provider.requests")
    def test_prefetch_timeout(self, mock_requests, provider):
        """Test prefetch handles timeout gracefully"""
        import requests as real_requests
        mock_requests.post.side_effect = real_requests.exceptions.Timeout()
        mock_requests.exceptions = real_requests.exceptions

        result = provider.prefetch("test query")
        assert result == ""

    @patch("ams_memory.provider.requests")
    def test_prefetch_non_200_returns_empty(self, mock_requests, provider):
        """J26: a gateway error degrades to no injected context, not a crash."""
        mock_response = Mock()
        mock_response.status_code = 503
        mock_requests.post.return_value = mock_response

        assert provider.prefetch("test query") == ""

    @patch("ams_memory.provider.requests")
    def test_sync_turn_success(self, mock_requests, provider):
        """Test successful turn sync (capture runs on the sync thread, U9)"""
        provider._session_id = "test-session"
        mock_response = Mock()
        mock_response.status_code = 200
        mock_response.json.return_value = {"status": "ok", "turns_saved": 2}
        mock_requests.post.return_value = mock_response

        with patch("ams_memory.provider.threading.Thread", _InlineThread):
            provider.sync_turn("user message", "assistant response")

        mock_requests.post.assert_called_once()
        call_args = mock_requests.post.call_args
        assert call_args[1]["json"]["session_id"] == "test-session"
        assert len(call_args[1]["json"]["turns"]) == 2

    @patch("ams_memory.provider.requests")
    def test_sync_turn_empty_content(self, mock_requests, provider):
        """Test sync_turn with empty content does nothing"""
        provider.sync_turn("", "")
        mock_requests.post.assert_not_called()

    @patch("ams_memory.provider.requests")
    def test_sync_turn_defers_capture_to_daemon_thread(self, mock_requests, provider):
        """U9: sync_turn returns before the HTTP call happens — a hung
        gateway can't add latency to the agent loop."""
        spawned = []

        class _RecorderThread(_InlineThread):
            def start(self):
                spawned.append(self)  # never runs the target

        provider._session_id = "test-session"
        with patch("ams_memory.provider.threading.Thread", _RecorderThread):
            provider.sync_turn("user", "assistant")

        # Returned without touching the network
        mock_requests.post.assert_not_called()
        assert len(spawned) == 1
        assert spawned[0].daemon is True

        # ...and the deferred work still performs the capture once run
        spawned[0]._target(*spawned[0]._args, **spawned[0]._kwargs)
        mock_requests.post.assert_called_once()
        assert mock_requests.post.call_args[1]["timeout"] == _CAPTURE_TIMEOUT == 3

    @patch("ams_memory.provider.requests")
    def test_sync_turn_auto_generates_session_id(self, mock_requests, provider):
        """Test sync_turn auto-generates session_id if not set"""
        mock_response = Mock()
        mock_response.status_code = 200
        mock_requests.post.return_value = mock_response

        with patch("ams_memory.provider.threading.Thread", _InlineThread):
            provider.sync_turn("user", "assistant")
        assert len(provider._session_id) == 36
        mock_requests.post.assert_called_once()  # thread really fired

    @patch("ams_memory.provider.requests")
    def test_sync_turn_timeout(self, mock_requests, provider):
        """Test sync_turn handles timeout gracefully (on the background thread)"""
        import requests as real_requests
        mock_requests.post.side_effect = real_requests.exceptions.Timeout()
        mock_requests.exceptions = real_requests.exceptions

        provider._session_id = "test-session"
        with patch("ams_memory.provider.threading.Thread", _InlineThread):
            provider.sync_turn("user", "assistant")
        # Should not raise; the capture was still attempted
        mock_requests.post.assert_called_once()

    def test_handle_tool_call_search(self, provider):
        """Test handle_tool_call for memory_search"""
        with patch.object(provider, "_tool_search") as mock_search:
            mock_search.return_value = {"memories": []}
            result = provider.handle_tool_call("memory_search", {"query": "test"})
            mock_search.assert_called_once_with({"query": "test"})
            assert result == '{"memories": []}'

    def test_handle_tool_call_save(self, provider):
        """Test handle_tool_call for memory_save"""
        with patch.object(provider, "_tool_save") as mock_save:
            mock_save.return_value = {"status": "saved"}
            result = provider.handle_tool_call("memory_save", {"content": "test"})
            mock_save.assert_called_once_with({"content": "test"})
            assert result == '{"status": "saved"}'

    def test_handle_tool_call_unknown(self, provider):
        """Test handle_tool_call with unknown tool raises NotImplementedError"""
        with pytest.raises(NotImplementedError):
            provider.handle_tool_call("unknown_tool", {})

    @patch("ams_memory.provider.requests")
    def test_on_session_end(self, mock_requests, provider):
        """Test session end notification"""
        provider._session_id = "test-session"
        mock_response = Mock()
        mock_response.status_code = 200
        mock_requests.post.return_value = mock_response

        provider.on_session_end([])

        mock_requests.post.assert_called_once()
        call_args = mock_requests.post.call_args
        assert "session/end" in call_args[0][0]
        # Synchronous path keeps the pre-U9 budget, not the 3s background one
        assert call_args[1]["timeout"] == _REQUEST_TIMEOUT == 10

    @patch("ams_memory.provider.requests")
    def test_on_session_end_joins_inflight_capture_first(self, mock_requests, provider):
        """Ordering: the final turn's background capture must land before
        /session/end fires, or the server-side pipeline misses it."""
        provider._session_id = "test-session"
        events = []

        class _FakeThread:
            def is_alive(self):
                return not events  # alive until join() records it

            def join(self, timeout=None):
                events.append(("join", timeout))

        provider._capture_thread = _FakeThread()
        mock_response = Mock()
        mock_response.status_code = 200

        def _post(*args, **kwargs):
            events.append(("post",))
            return mock_response

        mock_requests.post.side_effect = _post
        provider.on_session_end([])

        assert [e[0] for e in events] == ["join", "post"]
        assert events[0][1] == _CAPTURE_TIMEOUT + 1  # bounded join

    def test_on_session_switch(self, provider):
        """Test session switch handling"""
        provider._session_id = "old-session"

        provider.on_session_switch("new-session", reset=True)
        assert provider._session_id == "new-session"

    def test_format_memories(self, provider):
        """Test memory formatting"""
        memories = [
            {"content": "Memory 1", "layer": "L1", "type": "fact", "confidence": 0.9},
            {"content": "Memory 2", "layer": "L2", "type": "scenario", "confidence": 0.8},
        ]
        result = provider._format_memories(memories)
        assert "<recalled_memories>" in result
        assert "[Memory 1]" in result
        assert "Memory 1" in result
        assert "[Memory 2]" in result
        assert "Memory 2" in result
        assert "</recalled_memories>" in result

    def test_format_memories_empty(self, provider):
        """Test formatting empty memory list"""
        result = provider._format_memories([])
        assert result == ""

    def test_format_memories_omits_missing_confidence(self, provider):
        """J26: an entry without a confidence key must not render a
        misleading "confidence=0.00" — the segment is dropped instead."""
        memories = [{"content": "persona note", "layer": "L3", "type": "persona"}]
        result = provider._format_memories(memories)
        assert "confidence" not in result
        assert "[Memory 1] (L3/persona)" in result
        assert "persona note" in result

    def test_format_memories_renders_confidence_score(self, provider):
        """J26: L2/L3-style entries carrying confidence_score still show it."""
        memories = [
            {"content": "x", "layer": "L2", "type": "scenario", "confidence_score": 0.42},
        ]
        result = provider._format_memories(memories)
        assert "confidence=0.42" in result

    @patch("ams_memory.provider.requests")
    def test_tool_search_success(self, mock_requests, provider):
        """Test _tool_search successful search"""
        mock_response = Mock()
        mock_response.status_code = 200
        mock_response.json.return_value = {"memories": [{"content": "test"}]}
        mock_requests.post.return_value = mock_response

        result = provider._tool_search({"query": "test", "top_k": 3})
        assert result["memories"] == [{"content": "test"}]
        # Memory-poisoning mitigation: tool results carry untrusted-data framing
        assert "untrusted" in result["notice"]
        assert "do not follow" in result["notice"]

    @patch("ams_memory.provider.requests")
    def test_tool_search_non_200_shape(self, mock_requests, provider):
        """J26: failed search returns the documented error shape."""
        mock_response = Mock()
        mock_response.status_code = 503
        mock_requests.post.return_value = mock_response

        result = provider._tool_search({"query": "test"})
        assert result["memories"] == []
        assert "HTTP 503" in result["error"]

    @patch("ams_memory.provider.requests")
    def test_tool_search_exception_shape(self, mock_requests, provider):
        """J26: transport failure still returns the error shape, never raises."""
        mock_requests.post.side_effect = RuntimeError("boom")

        result = provider._tool_search({"query": "test"})
        assert result["memories"] == []
        assert "boom" in result["error"]

    @patch("ams_memory.provider.requests")
    def test_tool_save_success(self, mock_requests, provider):
        """Test _tool_save successful save"""
        provider._session_id = "test-session"
        mock_response = Mock()
        mock_response.status_code = 200
        mock_requests.post.return_value = mock_response

        result = provider._tool_save({"content": "test memory", "confidence": "high"})
        # U8: confidence is no longer echoed as if honored — the removed
        # parameter is ignored with an explanatory note.
        assert result["saved"] is True
        assert "confidence" not in result
        assert result["note"] == "confidence is managed by the memory server"
        # Synchronous tool path keeps the pre-U9 10s budget
        assert mock_requests.post.call_args[1]["timeout"] == _REQUEST_TIMEOUT
        turn = mock_requests.post.call_args[1]["json"]["turns"][0]
        # U8: content stored verbatim — no "[Memory saved]" prefix pollution,
        # and no fake fields the server would discard.
        assert turn["content"] == "test memory"
        assert turn["role"] == "system"
        assert set(turn) == {"role", "content", "timestamp"}

    @patch("ams_memory.provider.requests")
    def test_tool_save_without_confidence_has_no_note(self, mock_requests, provider):
        """U8: a plain save reports exactly what it did, nothing more."""
        provider._session_id = "test-session"
        mock_response = Mock()
        mock_response.status_code = 200
        mock_requests.post.return_value = mock_response

        result = provider._tool_save({"content": "fact"})
        assert result == {"saved": True}

    @patch("ams_memory.provider.requests")
    def test_tool_save_non_200_shape(self, mock_requests, provider):
        """J26: failed save returns the documented error shape."""
        provider._session_id = "test-session"
        mock_response = Mock()
        mock_response.status_code = 502
        mock_requests.post.return_value = mock_response

        result = provider._tool_save({"content": "fact"})
        assert result["saved"] is False
        assert "HTTP 502" in result["error"]

    @patch("ams_memory.provider.requests")
    def test_tool_save_exception_shape(self, mock_requests, provider):
        """J26: transport failure still returns the error shape, never raises."""
        provider._session_id = "test-session"
        mock_requests.post.side_effect = RuntimeError("boom")

        result = provider._tool_save({"content": "fact"})
        assert result["saved"] is False
        assert "boom" in result["error"]


class TestAutoRecallStore:
    """Test auto_recall and auto_store config flags"""

    @patch("ams_memory.provider.requests")
    def test_prefetch_disabled_when_auto_recall_false(self, mock_requests):
        provider = AMSMemoryProvider(config={"auto_recall": False})
        result = provider.prefetch("test query")
        assert result == ""
        mock_requests.post.assert_not_called()

    @patch("ams_memory.provider.requests")
    def test_sync_turn_disabled_when_auto_store_false(self, mock_requests):
        provider = AMSMemoryProvider(config={"auto_store": False})
        provider._session_id = "test-session"
        provider.sync_turn("user", "assistant")
        mock_requests.post.assert_not_called()

    @patch("ams_memory.provider.requests")
    def test_prefetch_enabled_by_default(self, mock_requests):
        provider = AMSMemoryProvider(config={})
        assert provider.auto_recall is True
        mock_response = Mock()
        mock_response.status_code = 200
        mock_response.json.return_value = {"memories": []}
        mock_requests.post.return_value = mock_response
        provider.prefetch("query")
        mock_requests.post.assert_called_once()

    @patch("ams_memory.provider.requests")
    def test_sync_turn_enabled_by_default(self, mock_requests):
        provider = AMSMemoryProvider(config={})
        assert provider.auto_store is True
        mock_response = Mock()
        mock_response.status_code = 200
        mock_requests.post.return_value = mock_response
        with patch("ams_memory.provider.threading.Thread", _InlineThread):
            provider.sync_turn("user", "assistant")
        mock_requests.post.assert_called_once()


class TestLoadConfig:
    """J26: _load_config env-var / JSON precedence coverage,
    including the J25 malformed-value regression anchors."""

    @pytest.fixture
    def hermes_home(self, tmp_path, monkeypatch):
        """Isolated $HERMES_HOME with all AMS_* env vars cleared."""
        home = tmp_path / ".hermes"
        home.mkdir()
        # Stub hermes_constants so _load_config resolves the home through our
        # tmp dir regardless of whether the real module is installed.
        stub = types.ModuleType("hermes_constants")
        stub.get_hermes_home = lambda: str(home)
        monkeypatch.setitem(sys.modules, "hermes_constants", stub)
        for var in ("AMS_GATEWAY_URL", "AMS_API_KEY", "AMS_RECALL_TOP_K",
                    "AMS_AUTO_RECALL", "AMS_AUTO_STORE"):
            monkeypatch.delenv(var, raising=False)
        return home

    def test_defaults_when_nothing_set(self, hermes_home):
        cfg = _load_config()
        assert cfg == {
            "gateway_url": "http://127.0.0.1:8765",
            "api_key": "",
            "recall_top_k": 5,
            "auto_recall": True,
            "auto_store": True,
        }

    def test_env_variables(self, hermes_home, monkeypatch):
        monkeypatch.setenv("AMS_GATEWAY_URL", "http://env-gw:9999")
        monkeypatch.setenv("AMS_API_KEY", "env-key")
        monkeypatch.setenv("AMS_RECALL_TOP_K", "7")
        monkeypatch.setenv("AMS_AUTO_RECALL", "false")
        monkeypatch.setenv("AMS_AUTO_STORE", "FALSE")
        cfg = _load_config()
        assert cfg["gateway_url"] == "http://env-gw:9999"
        assert cfg["api_key"] == "env-key"
        assert cfg["recall_top_k"] == 7
        assert cfg["auto_recall"] is False
        assert cfg["auto_store"] is False

    def test_json_overrides_env(self, hermes_home, monkeypatch):
        monkeypatch.setenv("AMS_GATEWAY_URL", "http://env-gw:9999")
        monkeypatch.setenv("AMS_API_KEY", "env-key")
        monkeypatch.setenv("AMS_RECALL_TOP_K", "7")
        (hermes_home / "ams.json").write_text(
            json.dumps({"gateway_url": "http://json-gw:1", "recall_top_k": 12}),
            encoding="utf-8",
        )
        cfg = _load_config()
        assert cfg["gateway_url"] == "http://json-gw:1"
        assert cfg["recall_top_k"] == 12
        # keys the file doesn't mention keep their env-var values
        assert cfg["api_key"] == "env-key"

    @pytest.mark.parametrize("raw", ["abc", "", "5.5"])
    def test_invalid_recall_top_k_falls_back_to_default(
        self, hermes_home, monkeypatch, raw
    ):
        """J25 regression: a malformed number must not crash plugin loading."""
        monkeypatch.setenv("AMS_RECALL_TOP_K", raw)
        cfg = _load_config()
        assert cfg["recall_top_k"] == 5

    def test_broken_json_falls_back_to_env(self, hermes_home, monkeypatch):
        monkeypatch.setenv("AMS_GATEWAY_URL", "http://env-gw:9999")
        (hermes_home / "ams.json").write_text("{ this is not json", encoding="utf-8")
        cfg = _load_config()
        assert cfg["gateway_url"] == "http://env-gw:9999"

    def test_non_dict_json_is_ignored(self, hermes_home):
        (hermes_home / "ams.json").write_text("[1, 2, 3]", encoding="utf-8")
        cfg = _load_config()
        assert cfg["gateway_url"] == "http://127.0.0.1:8765"


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
