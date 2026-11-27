use super::*;

fn int8_cosine(a: &[u8], b: &[u8]) -> f32 {
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (&xb, &yb) in a.iter().zip(b.iter()) {
        let x = (xb as i8) as f32;
        let y = (yb as i8) as f32;
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    dot / (na.sqrt() * nb.sqrt())
}

#[test]
fn test_l2_normalize_unit() {
    let mut v = vec![3.0, 4.0];
    l2_normalize(&mut v).unwrap();
    assert!((v[0] - 0.6).abs() < 1e-6);
    assert!((v[1] - 0.8).abs() < 1e-6);
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-6);
}

#[test]
fn test_l2_normalize_already_unit() {
    let mut v = vec![1.0, 0.0, 0.0];
    l2_normalize(&mut v).unwrap();
    assert!((v[0] - 1.0).abs() < 1e-6);
}

#[test]
fn test_l2_normalize_zero() {
    let mut v = vec![0.0, 0.0, 0.0];
    l2_normalize(&mut v).unwrap();
    assert_eq!(v, vec![0.0, 0.0, 0.0]);
}

#[test]
fn test_l2_normalize_rejects_non_finite() {
    let mut v_nan = vec![1.0, f32::NAN];
    assert!(l2_normalize(&mut v_nan).is_err());

    let mut v_inf = vec![f32::INFINITY, 0.0];
    assert!(l2_normalize(&mut v_inf).is_err());

    let mut v_neg_inf = vec![0.0, f32::NEG_INFINITY];
    assert!(l2_normalize(&mut v_neg_inf).is_err());
}

#[test]
fn test_quantize_absmax_proportional_scaling() {
    let vec = vec![0.1, 0.05, -0.025, 0.0];
    let q = quantize_to_int8(&vec);
    assert_eq!(q, vec![127u8, 64u8, (-32i8) as u8, 0u8]);
}

#[test]
fn test_quantize_positive() {
    let vec = vec![0.5; EMBEDDING_DIMENSIONS];
    let q = quantize_to_int8(&vec);
    assert_eq!(q.len(), EMBEDDING_DIMENSIONS);
    assert!(q.iter().all(|&b| b == 127u8));
}

#[test]
fn test_quantize_negative() {
    let vec = vec![-1.0; EMBEDDING_DIMENSIONS];
    let q = quantize_to_int8(&vec);
    assert_eq!(q.len(), EMBEDDING_DIMENSIONS);
    assert!(q.iter().all(|&b| b == (-127i8) as u8));
}

#[test]
fn test_quantize_zero() {
    let vec = vec![0.0; EMBEDDING_DIMENSIONS];
    let q = quantize_to_int8(&vec);
    assert_eq!(q.len(), EMBEDDING_DIMENSIONS);
    assert!(q.iter().all(|&b| b == 0u8));
}

#[test]
fn test_quantize_non_finite_returns_zeros() {
    let q_nan = quantize_to_int8(&[1.0, f32::NAN, 0.5]);
    assert_eq!(q_nan, vec![0u8, 0u8, 0u8]);

    let q_inf = quantize_to_int8(&[0.5, f32::INFINITY]);
    assert_eq!(q_inf, vec![0u8, 0u8]);
}

/// M5: with a subnormal max_abs the f32 scale overflowed to +inf and
/// flattened every component to ±127; f64 keeps the proportions.
#[test]
fn test_quantize_tiny_magnitudes_keep_proportionality() {
    let vec = vec![1e-38f32, -2.5e-39, 0.0];
    let q = quantize_to_int8(&vec);
    assert_eq!(q, vec![127u8, (-32i8) as u8, 0u8]);
}

#[test]
fn test_quantize_1024_dim_preserves_cosine_precision() {
    let mut a: Vec<f32> = (0..EMBEDDING_DIMENSIONS)
        .map(|i| ((i % 17) as f32 - 8.0) + 0.5)
        .collect();
    let mut b: Vec<f32> = (0..EMBEDDING_DIMENSIONS)
        .map(|i| ((i % 13) as f32 - 6.0) + 1.0)
        .collect();
    l2_normalize(&mut a).unwrap();
    l2_normalize(&mut b).unwrap();

    let cos_f32: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let qa = quantize_to_int8(&a);
    let qb = quantize_to_int8(&b);
    let cos_q = int8_cosine(&qa, &qb);
    assert!(
        (cos_f32 - cos_q).abs() < 0.005,
        "absmax INT8 cosine error too large: f32={cos_f32}, int8={cos_q}"
    );
}

/// 测试缺口：from_config 构造失败 → None（keyword-only 降级）。纯空白
/// api_url/api_model 的 is_empty() 为 false（绕过前置检查），但 ApiEmbedder::new
/// trim 后为空 → bail → from_config 返回 None。
#[test]
fn test_from_config_construction_failure_returns_none() {
    let mut cfg = crate::config::Config::default().embedding;
    cfg.api_url = "   ".to_string();
    cfg.api_model = "m".to_string();
    assert!(LazyEmbedder::from_config(&cfg).is_none());

    let mut cfg2 = crate::config::Config::default().embedding;
    cfg2.api_url = "https://x".to_string();
    cfg2.api_model = "  ".to_string();
    assert!(LazyEmbedder::from_config(&cfg2).is_none());
}

/// 离线 mock：按请求 input 文本（解析为轴序号）返回对应 axis 单位向量，
/// 用于验证 embed_batch_chunked 跨块顺序保持。
fn spawn_axis_echo_mock(dim: usize) -> std::net::SocketAddr {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut len = 0usize;
            let mut line = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line).unwrap_or(0);
                if n == 0 || line.trim_end().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            let _ = reader.read_exact(&mut body);
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let inputs = req
                .get("input")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut data = Vec::new();
            for (idx, item) in inputs.iter().enumerate() {
                let axis = item
                    .as_str()
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                let mut v = vec![0.0f32; dim];
                if axis < dim {
                    v[axis] = 1.0;
                }
                data.push(serde_json::json!({"embedding": v, "index": idx}));
            }
            let payload = serde_json::json!({"data": data}).to_string();
            let resp = format!(
                "HTTP/1.1 200 Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                payload.len(),
                payload
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    addr
}

/// 测试缺口：embed_batch_chunked 多块顺序——batch_size=2、5 个输入分 3 块，
/// 输出必须按输入顺序对齐（out[i] 为 axis i 向量），不得跨块错位/丢条。
#[test]
fn test_embed_documents_chunked_preserves_order() {
    let dim = 5usize;
    let addr = spawn_axis_echo_mock(dim);
    let mut cfg = crate::config::Config::default().embedding;
    cfg.api_url = format!("http://{addr}/v1");
    cfg.api_model = "m".to_string();
    cfg.dimensions = dim;
    cfg.batch_size = 2;
    let emb = LazyEmbedder::from_config(&cfg).expect("mock 端点构造成功");
    let out = emb.embed_documents(&["0", "1", "2", "3", "4"]).unwrap();
    assert_eq!(out.len(), 5, "分块不得丢条/重复");
    for (i, v) in out.iter().enumerate() {
        assert_eq!(v.len(), dim);
        assert!(
            (v[i] - 1.0).abs() < 1e-6,
            "out[{i}] 应为 axis {i} 向量: {v:?}"
        );
    }
}

fn decode_golden_f32(v: &serde_json::Value) -> f32 {
    if let Some(n) = v.as_f64() {
        return n as f32;
    }
    match v.as_str() {
        Some("NaN") => f32::NAN,
        Some("Infinity") => f32::INFINITY,
        Some("-Infinity") => f32::NEG_INFINITY,
        other => panic!("unexpected golden f32 token: {other:?}"),
    }
}

/// Phase 1c (`T1.9`) 黄金向量桥：直接校验 `quantize_to_int8`（含次正规数、负峰值、
/// 全零、NaN、±Inf、空切片）与 `l2_normalize -> quantize_to_int8` 对冻结样本
/// `tests/fixtures/golden/int8_quant_vectors.json` 的逐字节一致性。
#[test]
fn test_quantize_matches_golden_fixture() {
    let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("golden")
        .join("int8_quant_vectors.json");
    let raw = std::fs::read_to_string(&fixture_path)
        .unwrap_or_else(|e| panic!("读取 {}: {e}", fixture_path.display()));
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();

    let direct_cases = doc["direct_cases"].as_array().unwrap();
    assert!(
        direct_cases.len() >= 8,
        "direct_cases must cover all edge classes"
    );
    for case in direct_cases {
        let name = case["name"].as_str().unwrap();
        let input: Vec<f32> = case["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(decode_golden_f32)
            .collect();
        let expected: Vec<u8> = case["expected_u8"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| u8::try_from(n.as_u64().unwrap()).unwrap())
            .collect();
        assert_eq!(
            quantize_to_int8(&input),
            expected,
            "direct golden int8 mismatch on case `{name}`"
        );
    }

    let e2e_cases = doc["e2e_1024_cases"].as_array().unwrap();
    assert!(
        e2e_cases.len() >= 4,
        "e2e_1024_cases must cover non-zero and all-zero 1024d vectors"
    );
    for case in e2e_cases {
        let name = case["name"].as_str().unwrap();
        let mut input: Vec<f32> = case["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(decode_golden_f32)
            .collect();
        assert_eq!(input.len(), EMBEDDING_DIMENSIONS);
        l2_normalize(&mut input).unwrap();
        let expected: Vec<u8> = case["expected_u8"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| u8::try_from(n.as_u64().unwrap()).unwrap())
            .collect();
        assert_eq!(
            quantize_to_int8(&input),
            expected,
            "normalized 1024d golden int8 mismatch on case `{name}`"
        );
    }
}
