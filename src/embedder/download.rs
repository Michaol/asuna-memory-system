use crate::config::Config;
use std::path::{Path, PathBuf};
use std::io::Write;

/// 模型所需文件列表 (ONNX + Tokenizer)
#[allow(dead_code)]
pub const MODEL_FILES: &[(&str, &str)] = &[
    // ONNX 模型 (在 onnx/ 子目录)
    ("model_quantized.onnx", "onnx/model_quantized.onnx"),
    ("model_quantized.onnx_data", "onnx/model_quantized.onnx_data"),
    // Tokenizer (在仓库根目录)
    ("tokenizer.json", "tokenizer.json"),
    ("config.json", "config.json"),
    ("special_tokens_map.json", "special_tokens_map.json"),
    ("tokenizer_config.json", "tokenizer_config.json"),
];

/// HuggingFace 下载基础 URL
#[allow(dead_code)]
const HF_BASE: &str = "https://huggingface.co/onnx-community/embeddinggemma-300m-ONNX/resolve/main";

/// 检查模型目录是否完整
#[allow(dead_code)]
pub fn check_model_dir(dir: &Path) -> bool {
    if !dir.exists() {
        return false;
    }
    MODEL_FILES
        .iter()
        .all(|(local_name, _)| dir.join(local_name).exists())
}

/// 智能发现模型目录
#[allow(dead_code)]
pub fn discover_model(config: &Config) -> anyhow::Result<PathBuf> {
    if let Some(dir) = config.discover_model_dir() {
        if check_model_dir(&dir) {
            tracing::info!("发现模型目录: {}", dir.display());
            return Ok(dir);
        }
    }

    let asuna_models = config.data_dir.join("models").join("embeddinggemma-300m-q8");
    if check_model_dir(&asuna_models) {
        return Ok(asuna_models);
    }

    tracing::info!("未找到模型，将下载到: {}", asuna_models.display());
    std::fs::create_dir_all(&asuna_models)?;
    download_model(&asuna_models)?;
    Ok(asuna_models)
}

/// 下载模型文件（从 HF_TOKEN 环境变量读取认证 token）
fn download_model(dir: &Path) -> anyhow::Result<()> {
    let token = std::env::var("HF_TOKEN")
        .map_err(|_| anyhow::anyhow!("请设置 HF_TOKEN 环境变量以下载模型"))?;

    let client = reqwest::blocking::Client::new();
    for (local_name, hf_path) in MODEL_FILES {
        let url = format!("{}/{}", HF_BASE, hf_path);
        let dest = dir.join(local_name);
        if dest.exists() {
            tracing::info!("跳过已存在: {}", local_name);
            continue;
        }
        tracing::info!("下载: {} -> {}", url, dest.display());
        let resp = client.get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .send()?;
        if !resp.status().is_success() {
            anyhow::bail!("下载失败 {}: {}", url, resp.status());
        }
        // 检查 Content-Length 用于完整性校验
        let expected_size = resp.content_length();

        // 流式写入 .partial 临时文件，完成后 rename 到目标文件
        // 这样中断/崩溃只留 .partial 残骸，不会被后续 `dest.exists()` 误判为完成
        let partial = dest.with_extension("partial");
        let mut reader = resp;
        let bytes_written = {
            let mut file = std::fs::File::create(&partial)?;
            let written = std::io::copy(&mut reader, &mut file)?;
            file.flush()?;
            written
        };

        if let Some(expected) = expected_size {
            if bytes_written != expected {
                let _ = std::fs::remove_file(&partial);
                anyhow::bail!(
                    "下载不完整 {}: 期望 {} bytes, 实际 {} bytes",
                    local_name,
                    expected,
                    bytes_written
                );
            }
        }

        std::fs::rename(&partial, &dest)?;
        tracing::info!("完成: {} ({} bytes)", local_name, bytes_written);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_nonexistent_dir() {
        assert!(!check_model_dir(Path::new("/nonexistent/path")));
    }
}
