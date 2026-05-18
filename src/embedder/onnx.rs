use ort::execution_providers::CPUExecutionProvider;
use ort::session::builder::SessionBuilder;
use ort::session::Session;
use std::path::Path;
use std::sync::Once;

use super::tokenizer::Tokenizer;

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
            max_length: 2048,
            output_name,
            dimensions: 768,
        })
    }

    /// 生成单个文本嵌入
    pub fn embed(&mut self, text: &str) -> anyhow::Result<Vec<f32>> {
        let results = self.embed_batch(&[text])?;
        Ok(results.into_iter().next().unwrap_or_default())
    }

    /// 批量嵌入
    pub fn embed_batch(&mut self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let (all_ids, all_masks): (Vec<Vec<i64>>, Vec<Vec<i64>>) = texts
            .iter()
            .map(|t| self.tokenizer.encode(t, self.max_length))
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .unzip();

        let batch_size = texts.len();

        // 构造 ONNX 输入张量 [batch_size, max_length]
        let ids_flat: Vec<i64> = all_ids.into_iter().flatten().collect();
        let masks_flat: Vec<i64> = all_masks.into_iter().flatten().collect();

        let ids_array = ndarray::Array2::from_shape_vec((batch_size, self.max_length), ids_flat)?;
        let masks_array = ndarray::Array2::from_shape_vec((batch_size, self.max_length), masks_flat)?;

        // 使用 ort v2 inputs! 宏进行推理
        let outputs = self.session.run(ort::inputs![
            "input_ids" => ort::value::TensorRef::from_array_view(&ids_array)?,
            "attention_mask" => ort::value::TensorRef::from_array_view(&masks_array)?,
        ])?;

        // EmbeddingGemma 输出 shape: (batch, 768) — 已 pooled
        let (shape, data) = outputs[self.output_name.as_str()].try_extract_tensor::<f32>()?;

        // L2 normalize（输出已是 pooled 后的向量，无需 mean pooling）
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
        assert_eq!(q[0], 64u8); // (0.5 * 127).round() = 64
    }

    #[test]
    fn test_quantize_negative() {
        let vec = vec![-1.0; 768];
        let q = quantize_to_int8(&vec);
        assert_eq!(q[0], 129u8); // -127i8 as u8
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
        assert_eq!(q[0], 127u8); // 2.0 clamped to 1.0 → 127
        assert_eq!(q[1], 129u8); // -3.0 clamped to -1.0 → -127 as u8
    }
}
