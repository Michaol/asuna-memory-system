"""Tests for AMS Hermes Provider"""

import pytest
from unittest.mock import Mock, AsyncMock, patch

# Skip all tests if hermes is not installed
pytest.importorskip("hermes")

from ams_memory.provider import AMSProvider
from hermes.models.message import Message


@pytest.fixture
def mock_config():
    """Mock configuration for testing"""
    return {
        "gateway_url": "http://test-gateway:8765",
        "auto_recall": True,
        "auto_store": True,
        "recall_top_k": 5,
        "store_threshold": 0.7,
    }


@pytest.fixture
def provider(mock_config):
    """Create provider instance for testing"""
    return AMSProvider(mock_config)


class TestAMSProvider:
    """Test suite for AMSProvider"""

    def test_initialization(self, provider, mock_config):
        """Test provider initialization with config"""
        assert provider.gateway_url == mock_config["gateway_url"]
        assert provider.auto_recall == mock_config["auto_recall"]
        assert provider.auto_store == mock_config["auto_store"]
        assert provider.recall_top_k == mock_config["recall_top_k"]
        assert provider.store_threshold == mock_config["store_threshold"]

    def test_initialization_defaults(self):
        """Test provider initialization with default config"""
        provider = AMSProvider({})
        assert provider.gateway_url == "http://127.0.0.1:8765"
        assert provider.auto_recall is True
        assert provider.auto_store is True
        assert provider.recall_top_k == 5
        assert provider.store_threshold == 0.7

    @pytest.mark.asyncio
    async def test_initialize_creates_session(self, provider):
        """Test that initialize creates an aiohttp session"""
        with patch("ams_memory.provider.aiohttp") as mock_aiohttp:
            mock_session = AsyncMock()
            mock_aiohttp.ClientSession.return_value = mock_session

            await provider.initialize()

            mock_aiohttp.ClientSession.assert_called_once()
            assert provider.session == mock_session

    @pytest.mark.asyncio
    async def test_cleanup_closes_session(self, provider):
        """Test that cleanup closes the session"""
        mock_session = AsyncMock()
        provider.session = mock_session

        await provider.cleanup()

        mock_session.close.assert_called_once()
        assert provider.session is None

    def test_format_memories_empty(self, provider):
        """Test formatting empty memories list"""
        result = provider._format_memories([])
        assert result == ""

    def test_format_memories_single(self, provider):
        """Test formatting single memory"""
        memories = [
            {
                "content": "User prefers Rust",
                "memory_type": "preference",
                "confidence_score": 0.9,
            }
        ]

        result = provider._format_memories(memories)

        assert "<recalled_memories>" in result
        assert "[Memory 1]" in result
        assert "User prefers Rust" in result
        assert "preference" in result
        assert "0.90" in result
        assert "</recalled_memories>" in result

    def test_format_memories_with_evolution_chain(self, provider):
        """Test formatting memory with evolution chain"""
        memories = [
            {
                "content": "User prefers Python",
                "memory_type": "preference",
                "confidence_score": 0.8,
                "evolution_chain": [
                    {"content": "User prefers Java", "version": 1},
                    {"content": "User prefers Python", "version": 2},
                ],
            }
        ]

        result = provider._format_memories(memories)

        assert "Evolution: 2 versions" in result

    def test_format_memories_with_related(self, provider):
        """Test formatting memory with related memories"""
        memories = [
            {
                "content": "User works on web projects",
                "memory_type": "fact",
                "confidence_score": 0.85,
                "related_memories": [
                    {"content": "User knows JavaScript"},
                    {"content": "User uses React"},
                ],
            }
        ]

        result = provider._format_memories(memories)

        assert "Related: 2 memories" in result

    def test_insert_after_system_with_system_message(self, provider):
        """Test inserting message after system message"""
        messages = [
            Message(role="system", content="You are a helpful assistant"),
            Message(role="user", content="Hello"),
        ]

        new_msg = Message(role="system", content="Additional context")
        result = provider._insert_after_system(messages, new_msg)

        assert len(result) == 3
        assert result[0].content == "You are a helpful assistant"
        assert result[1].content == "Additional context"
        assert result[2].content == "Hello"

    def test_insert_after_system_without_system_message(self, provider):
        """Test inserting message when no system message exists"""
        messages = [
            Message(role="user", content="Hello"),
            Message(role="assistant", content="Hi there"),
        ]

        new_msg = Message(role="system", content="Context")
        result = provider._insert_after_system(messages, new_msg)

        assert len(result) == 3
        assert result[0].content == "Context"
        assert result[1].content == "Hello"
        assert result[2].content == "Hi there"

    @pytest.mark.asyncio
    async def test_before_response_no_auto_recall(self, provider):
        """Test before_response when auto_recall is disabled"""
        provider.auto_recall = False
        messages = [Message(role="user", content="Hello")]

        result = await provider.before_response(messages)

        assert result == messages

    @pytest.mark.asyncio
    async def test_before_response_no_session(self, provider):
        """Test before_response when session is not initialized"""
        provider.session = None
        messages = [Message(role="user", content="Hello")]

        result = await provider.before_response(messages)

        assert result == messages

    @pytest.mark.asyncio
    async def test_after_response_no_auto_store(self, provider):
        """Test after_response when auto_store is disabled"""
        provider.auto_store = False
        messages = [Message(role="user", content="Hello")]
        response = Message(role="assistant", content="Hi")

        await provider.after_response(messages, response)

        # Should not make any HTTP calls
        assert provider.session is None

    @pytest.mark.asyncio
    async def test_after_response_no_session(self, provider):
        """Test after_response when session is not initialized"""
        provider.session = None
        messages = [Message(role="user", content="Hello")]
        response = Message(role="assistant", content="Hi")

        await provider.after_response(messages, response)

        # Should not raise any errors


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
