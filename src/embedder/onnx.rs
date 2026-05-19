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
    #[allow(dead_code)]
    dimensions: usize,
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

        // 动态检测输出 tensor name
        let output_name = session
            .outputs
            .first()
            .map(|o| o.name.clone())
            .unwrap_or_else(|| "sentence_embedding".to_string());
        tracing::info!("ONNX 输出张量: {}", output_name);

        let tokenizer = Tokenizer::load(model_dir)?;

        Ok(Self {
            session,
            tokenizer,
            // EmbeddingGemma 上限 2048，但保存 preview 仅 200~512 字符（视配置），
            // 这里取一个安全上界，实际推理按 batch 内最长动态 pad。
            max_length: 2048,
            output_name,
            dimensions: 768,
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

        let ids_array = ndarray::Array2::from_shape_vec((batch_size, batch_max), ids_flat)?;
        let masks_array = ndarray::Array2::from_shape_vec((batch_size, batch_max), masks_flat)?;

        let outputs = self.session.run(ort::inputs![
            "input_ids" => ort::value::TensorRef::from_array_view(&ids_array)?,
            "attention_mask" => ort::value::TensorRef::from_array_view(&masks_array)?,
        ])?;

        // EmbeddingGemma 输出 shape: (batch, 768) — 已 pooled
        let (shape, data) = outputs[self.output_name.as_str()].try_extract_tensor::<f32>()?;

        let hidden = shape[1] as usize;
        let mut results = Vec::with_capacity(batch_size);

        for b in 0..batch_size {
            let offset = b * hidden;
            let mut vec = data[offset..offset + hidden].to_vec();

            // L2 normalize
            let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in vec.iter_mut() {
                    *x /= norm;
                }
            }

            results.push(vec);
        }

        Ok(results)
    }

    #[allow(dead_code)]
    pub fn dimensions(&self) -> usize {
        self.dimensions
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
}
