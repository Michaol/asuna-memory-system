"""Tests for AMS Hermes MemoryProvider"""

import pytest
from unittest.mock import Mock, patch

from ams_memory.provider import AMSMemoryProvider


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
        assert provider._turn_seq == 0
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
        assert provider._turn_seq == 0

    def test_initialize_auto_generates_session_id(self, provider):
        """Test initialize auto-generates session_id if not provided"""
        provider.initialize()
        assert len(provider._session_id) == 36  # UUID format

    def test_system_prompt_block(self, provider):
        """Test system prompt block content"""
        prompt = provider.system_prompt_block()
        assert "persistent memory system" in prompt
        assert "past conversations" in prompt

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
        assert "confidence" in save_schema["parameters"]["properties"]

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
    def test_sync_turn_success(self, mock_requests, provider):
        """Test successful turn sync"""
        provider._session_id = "test-session"
        mock_response = Mock()
        mock_response.status_code = 200
        mock_response.json.return_value = {"status": "ok", "turns_saved": 2}
        mock_requests.post.return_value = mock_response

        provider.sync_turn("user message", "assistant response")

        assert provider._turn_seq == 1
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
    def test_sync_turn_auto_generates_session_id(self, mock_requests, provider):
        """Test sync_turn auto-generates session_id if not set"""
        mock_response = Mock()
        mock_response.status_code = 200
        mock_requests.post.return_value = mock_response

        provider.sync_turn("user", "assistant")
        assert len(provider._session_id) == 36

    @patch("ams_memory.provider.requests")
    def test_sync_turn_timeout(self, mock_requests, provider):
        """Test sync_turn handles timeout gracefully"""
        import requests as real_requests
        mock_requests.post.side_effect = real_requests.exceptions.Timeout()
        mock_requests.exceptions = real_requests.exceptions

        provider._session_id = "test-session"
        provider.sync_turn("user", "assistant")
        # Should not raise

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

    def test_on_session_switch(self, provider):
        """Test session switch handling"""
        provider._session_id = "old-session"
        provider._turn_seq = 5

        provider.on_session_switch("new-session", reset=True)
        assert provider._session_id == "new-session"
        assert provider._turn_seq == 0

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

    @patch("ams_memory.provider.requests")
    def test_tool_search_success(self, mock_requests, provider):
        """Test _tool_search successful search"""
        mock_response = Mock()
        mock_response.status_code = 200
        mock_response.json.return_value = {"memories": [{"content": "test"}]}
        mock_requests.post.return_value = mock_response

        result = provider._tool_search({"query": "test", "top_k": 3})
        assert result == {"memories": [{"content": "test"}]}

    @patch("ams_memory.provider.requests")
    def test_tool_save_success(self, mock_requests, provider):
        """Test _tool_save successful save"""
        provider._session_id = "test-session"
        mock_response = Mock()
        mock_response.status_code = 200
        mock_requests.post.return_value = mock_response

        result = provider._tool_save({"content": "test memory", "confidence": "high"})
        assert result == {"status": "saved", "confidence": "high"}


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
