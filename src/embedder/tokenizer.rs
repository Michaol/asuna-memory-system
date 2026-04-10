use std::path::Path;

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

    /// 编码文本，返回 input_ids + attention_mask
    pub fn encode(&self, text: &str, max_length: usize) -> anyhow::Result<(Vec<i64>, Vec<i64>)> {
        let encoding = self.inner.encode(
            format!("query: {}", text), // E5 模型需要 query 前缀
            true,
        ).map_err(|e| anyhow::anyhow!("tokenizer encode 失败: {}", e))?;

        let mut ids: Vec<i64> = encoding.get_ids().iter().map(|&x| x as i64).collect();
        let mut mask: Vec<i64> = encoding.get_attention_mask().iter().map(|&x| x as i64).collect();

        // 截断或填充到 max_length
        ids.truncate(max_length);
        mask.truncate(max_length);
        while ids.len() < max_length {
            ids.push(0);
            mask.push(0);
        }

        Ok((ids, mask))
    }

    /// 批量编码
    pub fn encode_batch(&self, texts: &[&str], max_length: usize) -> anyhow::Result<(Vec<Vec<i64>>, Vec<Vec<i64>>)> {
        let results: anyhow::Result<Vec<_>> = texts.iter().map(|t| self.encode(t, max_length)).collect();
        let pairs = results?;
        let (ids, masks) = pairs.into_iter().unzip();
        Ok((ids, masks))
    }
}
