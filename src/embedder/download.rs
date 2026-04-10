use crate::config::Config;
use std::path::{Path, PathBuf};

/// 模型所需文件列表
#[allow(dead_code)]
pub const MODEL_FILES: &[(&str, &str)] = &[
    ("model_O4.onnx", "onnx/model_O4.onnx"),
    ("tokenizer.json", "tokenizer.json"),
    ("config.json", "config.json"),
    ("special_tokens_map.json", "special_tokens_map.json"),
    ("tokenizer_config.json", "tokenizer_config.json"),
];

/// HuggingFace 下载基础 URL
#[allow(dead_code)]
const HF_BASE: &str = "https://huggingface.co/intfloat/multilingual-e5-small/resolve/main";

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

/// 智发现模型目录（使用 Config 中的逻辑）
#[allow(dead_code)]
pub fn discover_model(config: &Config) -> anyhow::Result<PathBuf> {
    if let Some(dir) = config.discover_model_dir() {
        if check_model_dir(&dir) {
            tracing::info!("发现模型目录: {}", dir.display());
            return Ok(dir);
        }
    }

    // 使用 asuna 自己的缓存目录
    let asuna_models = config.data_dir.join("models").join("multilingual-e5-small");
    if check_model_dir(&asuna_models) {
        return Ok(asuna_models);
    }

    // 需要下载
    tracing::info!("未找到模型，将下载到: {}", asuna_models.display());
    std::fs::create_dir_all(&asuna_models)?;
    download_model(&asuna_models)?;
    Ok(asuna_models)
}

/// 下载模型文件
#[allow(dead_code)]
fn download_model(dir: &Path) -> anyhow::Result<()> {
    for (local_name, hf_path) in MODEL_FILES {
        let url = format!("{}/{}", HF_BASE, hf_path);
        let dest = dir.join(local_name);
        if dest.exists() {
            tracing::info!("跳过已存在: {}", local_name);
            continue;
        }
        tracing::info!("下载: {} -> {}", url, dest.display());
        let resp = reqwest::blocking::get(&url)?;
        if !resp.status().is_success() {
            anyhow::bail!("下载失败 {}: {}", url, resp.status());
        }
        let bytes = resp.bytes()?;
        std::fs::write(&dest, &bytes)?;
        tracing::info!("完成: {} ({} bytes)", local_name, bytes.len());
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
