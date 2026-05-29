//! 模型文件下载：从 GitHub Release Assets 下载 EmbeddingGemma ONNX + Tokenizer。

use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::time::Duration;

/// GitHub 仓库（Release Assets 来源）
const GH_REPO: &str = "Michaol/asuna-memory-system";

/// 模型文件名 + 期望大小（bytes）+ SHA256 哈希。
/// 大小用于校验下载完整性，SHA256 用于校验完整性（防中间人篡改）。
/// 若 SHA256 为 None，仅校验大小（向后兼容，hash 待发布后补填）。
pub const MODEL_FILES: &[(&str, u64, Option<&str>)] = &[
    ("model_quantized.onnx", 3_347_993, None),
    ("model_quantized.onnx_data", 302_010_368, None),
    ("tokenizer.json", 17_518_607, None),
    ("tokenizer_config.json", 20_671, None),
    ("config.json", 1_308, None),
    ("special_tokens_map.json", 2_432, None),
];

/// 检查模型目录完整性：所有文件存在且大小不低于期望值
pub fn model_check(dir: &Path) -> bool {
    if !dir.exists() {
        return false;
    }
    MODEL_FILES.iter().all(|(name, expected, _)| {
        let path = dir.join(name);
        match std::fs::metadata(&path) {
            Ok(m) => m.len() >= *expected,
            Err(_) => false,
        }
    })
}

/// 从 GitHub Release 下载全部模型文件到 dest_dir。
/// 使用当前代码版本对应的 Release tag (v{CARGO_PKG_VERSION})。
/// progress 回调：参数 0.0~1.0，每完成一个文件触发一次。
pub fn download_model<P: FnMut(f64)>(
    dest_dir: &Path,
    mut progress: Option<P>,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(dest_dir)?;
    let tag = format!("v{}", env!("CARGO_PKG_VERSION"));

    // 自定义 Agent：300 秒超时（ureq 默认 30s 对 288MB 文件不够）
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(300))
        .build();

    for (i, (name, expected_size, expected_sha256)) in MODEL_FILES.iter().enumerate() {
        let url = format!(
            "https://github.com/{repo}/releases/download/{tag}/{file}",
            repo = GH_REPO,
            tag = tag,
            file = name,
        );
        download_file(&agent, &url, &dest_dir.join(name), *expected_size, *expected_sha256)?;
        if let Some(ref mut cb) = progress {
            cb((i + 1) as f64 / MODEL_FILES.len() as f64);
        }
    }
    Ok(())
}

/// 单文件下载：.partial 原子写入 + Content-Length 校验 + 可选 SHA256 校验
fn download_file(
    agent: &ureq::Agent,
    url: &str,
    dest: &Path,
    expected_size: u64,
    expected_sha256: Option<&str>,
) -> anyhow::Result<()> {
    let file_name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?");

    let resp = agent.get(url).call()?;

    let content_length: u64 = resp
        .header("Content-Length")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let partial = dest.with_extension("partial");
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(&partial)?;
    let mut buf = [0u8; 65536];
    let mut written: u64 = 0;
    let mut hasher = Sha256::new();

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        std::io::Write::write_all(&mut file, chunk)?;
        hasher.update(chunk);
        written += n as u64;
        if content_length > 0 {
            print!(
                "\r  {} {:.1}% ({}/{})",
                file_name,
                written as f64 / content_length as f64 * 100.0,
                fmt_bytes(written),
                fmt_bytes(content_length),
            );
        } else {
            print!("\r  {} {}", file_name, fmt_bytes(written));
        }
        std::io::Write::flush(&mut std::io::stdout())?;
    }
    println!();

    if written < expected_size {
        let _ = std::fs::remove_file(&partial);
        anyhow::bail!(
            "下载不完整 {}: {} bytes (期望 >= {} bytes)",
            file_name,
            written,
            expected_size
        );
    }

    // SHA256 verification (when hash is provided)
    if let Some(expected_hash) = expected_sha256 {
        let actual_hash = format!("{:x}", hasher.finalize());
        if actual_hash != expected_hash {
            let _ = std::fs::remove_file(&partial);
            anyhow::bail!(
                "SHA256 校验失败 {}: 期望 {}, 实际 {}",
                file_name,
                expected_hash,
                actual_hash
            );
        }
        tracing::info!("SHA256 校验通过: {}", file_name);
    }

    std::fs::rename(&partial, dest)?;
    Ok(())
}

fn fmt_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.0} KB", n as f64 / KB as f64)
    } else {
        format!("{} B", n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_check_nonexistent() {
        assert!(!model_check(Path::new("/nonexistent/path")));
    }

    #[test]
    fn test_model_check_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!model_check(tmp.path()));
    }

    #[test]
    fn test_fmt_bytes() {
        assert_eq!(fmt_bytes(500), "500 B");
        assert_eq!(fmt_bytes(1024), "1 KB");
        assert_eq!(fmt_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(fmt_bytes(302_010_368), "288.0 MB");
    }
}
