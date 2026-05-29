use std::path::Path;

/// EmbeddingGemma 任务类型 —— 决定输入前缀，影响向量空间
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedTask {
    /// 检索查询侧
    Query,
    /// 索引文档侧（保存对话 / rebuild 时使用）
    Document,
}

impl EmbedTask {
    fn format(self, text: &str) -> String {
        match self {
            // 官方双前缀方案（EmbeddingGemma 模型卡）：
            EmbedTask::Query => format!("task: search result | query: {}", text),
            EmbedTask::Document => format!("title: none | text: {}", text),
        }
    }
}

/// 包装 HuggingFace tokenizer
pub struct Tokenizer {
    inner: tokenizers::Tokenizer,
}

impl Tokenizer {
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join("tokenizer.json");
        let inner = tokenizers::Tokenizer::from_file(&path)
            .map_err(|e| anyhow::anyhow!("加载 tokenizer 失败: {}", e))?;
        Ok(Self { inner })
    }

    /// 编码文本到 input_ids + attention_mask（不做 padding，由调用方按 batch 内最长动态 pad）
    pub fn encode(
        &self,
        text: &str,
        task: EmbedTask,
        max_length: usize,
    ) -> anyhow::Result<(Vec<i64>, Vec<i64>)> {
        let encoding = self
            .inner
            .encode(task.format(text), true)
            .map_err(|e| anyhow::anyhow!("tokenizer encode 失败: {}", e))?;

        let mut ids: Vec<i64> = encoding.get_ids().iter().map(|&x| x as i64).collect();
        let mut mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&x| x as i64)
            .collect();

        ids.truncate(max_length);
        mask.truncate(max_length);
        Ok((ids, mask))
    }
}
