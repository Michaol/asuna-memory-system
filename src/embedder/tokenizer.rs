use std::borrow::Cow;
use std::path::Path;

/// 分词前的字符级预截断（J29）：只是 CPU/内存 DoS 上界——把 tokenizers 的
/// O(输入长度) 工作压到 O(4·max_length)。精确的 max_length token 截断仍由
/// encode 中的 `ids.truncate` 完成。4x 冗余的依据：常规语料平均 1 token ≈
/// ≤4 chars（英文 subword ~3-4、CJK 1-2），取前 4·max_length 字符足以覆盖前
/// max_length 个 token；病态输入（超长词/罕见合并）下可能比"先全量分词再截
/// 断"少喂一点尾部内容，属可接受的保守近似。未超预算的输入零拷贝借用返回。
fn pretruncate(text: &str, max_chars: usize) -> Cow<'_, str> {
    if text.len() <= max_chars {
        // 字节数 ≥ 字符数：字节层面未超预算则字符层面必然也未超
        return Cow::Borrowed(text);
    }
    match text.chars().nth(max_chars) {
        None => Cow::Borrowed(text), // 恰为 max_chars 个字符
        Some(_) => Cow::Owned(text.chars().take(max_chars).collect()),
    }
}

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
    ///
    /// 超长输入先经 `pretruncate` 做字符级预截断（DoS 上界，见其注释），
    /// 再分词、再精确截到 `max_length` 个 token。
    pub fn encode(
        &self,
        text: &str,
        task: EmbedTask,
        max_length: usize,
    ) -> anyhow::Result<(Vec<i64>, Vec<i64>)> {
        // 4x 冗余系数：理由见 pretruncate。用 saturating_mul 防 max_length 极大时溢出。
        let text = pretruncate(text, max_length.saturating_mul(4));
        let encoding = self
            .inner
            .encode(task.format(&text), true)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pretruncate_short_input_borrowed() {
        let t = pretruncate("hello", 100);
        assert!(matches!(t, Cow::Borrowed(_)), "under budget → zero-copy");
        assert_eq!(&*t, "hello");

        // 预算 0：任何非空输入都超限
        assert_eq!(&*pretruncate("a", 0), "");
        assert!(matches!(pretruncate("", 0), Cow::Borrowed(_)));
    }

    #[test]
    fn test_pretruncate_counts_chars_not_bytes() {
        // 5 个 CJK 字符 = 15 字节；max_chars=5 → 恰在预算内，不得拷贝/截断
        let s = "中".repeat(5);
        let t = pretruncate(&s, 5);
        assert!(
            matches!(t, Cow::Borrowed(_)),
            "exactly max_chars chars must not be truncated"
        );
        assert_eq!(&*t, s);

        // max_chars=4 → 截断为前 4 个字符（不是 4 字节，不得切断 UTF-8 序列）
        let t = pretruncate(&s, 4);
        assert!(matches!(t, Cow::Owned(_)));
        assert_eq!(&*t, "中中中中");
    }

    #[test]
    fn test_pretruncate_huge_input_is_bounded() {
        // 1MB 单轮输入（J29 DoS 场景）：预截断必须限时限内存地收敛到预算字符数
        let big = "字".repeat(1_000_000); // ~3MB UTF-8
        let t = pretruncate(&big, 4096);
        assert_eq!(t.chars().count(), 4096);
        assert!(matches!(t, Cow::Owned(_)));

        // 纯 ASCII 大输入走同一上界
        let big = "a".repeat(10_000_000);
        let t = pretruncate(&big, 2048);
        assert_eq!(t.len(), 2048);
    }
}
