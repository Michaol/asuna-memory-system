use ort::execution_providers::CPUExecutionProvider;
use ort::session::builder::SessionBuilder;
use ort::session::Session;
use std::path::Path;
use std::sync::Once;

use super::tokenizer::{EmbedTask, Tokenizer};

/// 确保 ONNX Runtime 只初始化一次
static ORT_INIT: Once = Once::new();

/// ONNX 推理嵌入器
pub struct OnnxEmbedder {
    session: Session,
    tokenizer: Tokenizer,
    max_length: usize,
    output_name: String,
    /// true = sentence_embedding (2D pooled), false = last_hidden_state (3D, needs mean pooling)
    is_pooled: bool,
}

impl OnnxEmbedder {
    pub fn new(model_dir: &Path) -> anyhow::Result<Self> {
        // 初始化 ONNX Runtime（仅首次）
        ORT_INIT.call_once(|| {
            let _ = ort::init()
                .with_execution_providers([CPUExecutionProvider::default().build()])
                .commit();
        });

        let onnx_path = model_dir.join("model_quantized.onnx");
        tracing::info!("加载 ONNX 模型: {}", onnx_path.display());

        let session = SessionBuilder::new()?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)?
            .commit_from_file(&onnx_path)?;

        // 优先选择 sentence_embedding（2D pooled 输出），
        // 回退到 last_hidden_state（3D，需 mean pooling）。
        // EmbeddingGemma 模型有两个输出：
        //   0: last_hidden_state  [batch, seq_len, 768] — 变长，不能直接用
        //   1: sentence_embedding [batch, 768]          — 固定 768 维，正确
        let output_name = session
            .outputs
            .iter()
            .find(|o| o.name == "sentence_embedding")
            .or_else(|| session.outputs.first())
            .map(|o| o.name.clone())
            .unwrap_or_else(|| "sentence_embedding".to_string());

        let is_pooled = output_name == "sentence_embedding";
        tracing::info!(
            "ONNX 输出张量: {} (pooled={})",
            output_name,
            is_pooled
        );

        let tokenizer = Tokenizer::load(model_dir)?;

        Ok(Self {
            session,
            tokenizer,
            // EmbeddingGemma 上限 2048，但保存 preview 仅 200~512 字符（视配置），
            // 这里取一个安全上界，实际推理按 batch 内最长动态 pad。
            max_length: 2048,
            output_name,
            is_pooled,
        })
    }

    /// 单文本嵌入（task 标识查询/文档）
    pub fn embed(&mut self, text: &str, task: EmbedTask) -> anyhow::Result<Vec<f32>> {
        let results = self.embed_batch(&[text], task)?;
        Ok(results.into_iter().next().unwrap_or_default())
    }

    /// 批量嵌入（按 batch 内最长长度动态 pad，避免恒定填充到 max_length 浪费算力）
    pub fn embed_batch(&mut self, texts: &[&str], task: EmbedTask) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        // 1. tokenize 全部
        let encoded: Vec<(Vec<i64>, Vec<i64>)> = texts
            .iter()
            .map(|t| self.tokenizer.encode(t, task, self.max_length))
            .collect::<anyhow::Result<_>>()?;

        // 2. 求 batch 内最长长度（至少 1，避免空 tensor）
        let batch_max = encoded
            .iter()
            .map(|(ids, _)| ids.len())
            .max()
            .unwrap_or(1)
            .max(1);

        let batch_size = texts.len();

        // 3. 动态 pad 到 batch_max（不再恒定 2048）
        let (ids_flat, masks_flat) = pad_encoded_batch(encoded, batch_max);

        let ids_array = ndarray::Array2::from_shape_vec((batch_size, batch_max), ids_flat)?;
        // clone masks_flat 供 3D mean pooling 使用（ArrayView2 不可行：
        // session.run 返回的 outputs 生命周期可能约束输入借用，NLL 无法释放）
        let masks_flat_copy = masks_flat.clone();
        let masks_array = ndarray::Array2::from_shape_vec((batch_size, batch_max), masks_flat)?;

        let outputs = self.session.run(ort::inputs![
            "input_ids" => ort::value::TensorRef::from_array_view(&ids_array)?,
            "attention_mask" => ort::value::TensorRef::from_array_view(&masks_array)?,
        ])?;

        let (shape, data) = outputs[self.output_name.as_str()].try_extract_tensor::<f32>()?;
        let rank = shape.len();

        tracing::debug!(
            "ONNX 输出: name={}, rank={}, shape={:?}, pooled={}",
            self.output_name,
            rank,
            shape,
            self.is_pooled
        );

        collect_results(
            shape,
            data,
            &masks_flat_copy,
            batch_size,
            batch_max,
            self.is_pooled,
        )
    }
}

/// 把 batch 内每条文本的 (token_ids, attention_mask) pad 到 batch_max 后展平
fn pad_encoded_batch(encoded: Vec<(Vec<i64>, Vec<i64>)>, batch_max: usize) -> (Vec<i64>, Vec<i64>) {
    let batch_size = encoded.len();
    let mut ids_flat: Vec<i64> = Vec::with_capacity(batch_size * batch_max);
    let mut masks_flat: Vec<i64> = Vec::with_capacity(batch_size * batch_max);
    for (mut ids, mut mask) in encoded {
        while ids.len() < batch_max {
            ids.push(0);
            mask.push(0);
        }
        ids_flat.extend(ids);
        masks_flat.extend(mask);
    }
    (ids_flat, masks_flat)
}

/// 按输出张量形状分派嵌入提取（2D 已 pooled / 3D 需 mean pooling）
fn collect_results(
    shape: &[i64],
    data: &[f32],
    masks_flat: &[i64],
    batch_size: usize,
    batch_max: usize,
    is_pooled: bool,
) -> anyhow::Result<Vec<Vec<f32>>> {
    let rank = shape.len();
    if is_pooled || rank == 2 {
        Ok(collect_pooled_output(shape, data, batch_size))
    } else if rank == 3 {
        Ok(collect_mean_pooled_output(
            shape, data, masks_flat, batch_size, batch_max,
        ))
    } else {
        anyhow::bail!(
            "unexpected ONNX output rank: {} (expected 2 or 3), shape: {:?}",
            rank,
            shape
        );
    }
}

/// sentence_embedding: (batch, hidden_dim) — 已 pooled
fn collect_pooled_output(shape: &[i64], data: &[f32], batch_size: usize) -> Vec<Vec<f32>> {
    let hidden = shape[1] as usize;
    let mut results = Vec::with_capacity(batch_size);
    for b in 0..batch_size {
        let offset = b * hidden;
        let mut vec = data[offset..offset + hidden].to_vec();
        l2_normalize(&mut vec);
        results.push(vec);
    }
    results
}

/// last_hidden_state: (batch, seq_len, hidden_dim) — 需 masked mean pooling
fn collect_mean_pooled_output(
    shape: &[i64],
    data: &[f32],
    masks_flat: &[i64],
    batch_size: usize,
    batch_max: usize,
) -> Vec<Vec<f32>> {
    let seq_len = shape[1] as usize;
    let hidden = shape[2] as usize;
    // 防御性边界：模型 seq_len 可能因额外 special tokens 超出 batch_max，
    // 截断到两者最小值以保证 masks_flat 和 data 索引不越界
    let pool_len = seq_len.min(batch_max);

    let mut results = Vec::with_capacity(batch_size);
    for b in 0..batch_size {
        results.push(mean_pool_one(
            data, masks_flat, b, seq_len, hidden, pool_len, batch_max,
        ));
    }
    results
}

/// 单条文本的 masked mean pooling + L2 归一化
fn mean_pool_one(
    data: &[f32],
    masks_flat: &[i64],
    b: usize,
    seq_len: usize,
    hidden: usize,
    pool_len: usize,
    batch_max: usize,
) -> Vec<f32> {
    let mut pooled = vec![0.0f32; hidden];
    let mut valid_tokens = 0u32;

    for s in 0..pool_len {
        // masks_flat 索引: b * batch_max + s（安全：s < batch_max）
        if masks_flat[b * batch_max + s] == 1 {
            let offset = b * seq_len * hidden + s * hidden;
            for h in 0..hidden {
                pooled[h] += data[offset + h];
            }
            valid_tokens += 1;
        }
    }

    if valid_tokens > 0 {
        let inv = 1.0 / valid_tokens as f32;
        for v in pooled.iter_mut() {
            *v *= inv;
        }
    }

    l2_normalize(&mut pooled);
    pooled
}

/// L2 归一化（就地），零向量保持不变
fn l2_normalize(vec: &mut [f32]) {
    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        let inv = 1.0 / norm;
        for x in vec.iter_mut() {
            *x *= inv;
        }
    }
}

/// 将 L2 归一化的 f32 向量量化为 INT8 存储格式
pub fn quantize_to_int8(vec: &[f32]) -> Vec<u8> {
    vec.iter()
        .map(|&v| {
            let q = (v.clamp(-1.0, 1.0) * 127.0).round() as i8;
            q as u8
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quantize_positive() {
        let vec = vec![0.5; 768];
        let q = quantize_to_int8(&vec);
        assert_eq!(q.len(), 768);
        assert_eq!(q[0], 64u8);
    }

    #[test]
    fn test_quantize_negative() {
        let vec = vec![-1.0; 768];
        let q = quantize_to_int8(&vec);
        assert_eq!(q[0], 129u8);
    }

    #[test]
    fn test_quantize_zero() {
        let vec = vec![0.0; 768];
        let q = quantize_to_int8(&vec);
        assert_eq!(q[0], 0u8);
    }

    #[test]
    fn test_quantize_clamp() {
        let vec = vec![2.0, -3.0];
        let q = quantize_to_int8(&vec);
        assert_eq!(q[0], 127u8);
        assert_eq!(q[1], 129u8);
    }

    #[test]
    fn test_l2_normalize_unit() {
        let mut v = vec![3.0, 4.0];
        l2_normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6);
        assert!((v[1] - 0.8).abs() < 1e-6);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_l2_normalize_zero() {
        let mut v = vec![0.0, 0.0, 0.0];
        l2_normalize(&mut v);
        assert_eq!(v, vec![0.0, 0.0, 0.0]); // 零向量不变，无 NaN
    }

    #[test]
    fn test_l2_normalize_already_unit() {
        let mut v = vec![1.0, 0.0, 0.0];
        l2_normalize(&mut v);
        assert!((v[0] - 1.0).abs() < 1e-6);
    }
}
