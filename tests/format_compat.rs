//! Phase 1c (`T1.9`): Byte-level disk format contracts & v2.7.1 dual-variant upgrade suite.
//!
//! Deliberately avoids external FTS5 tokenizer crate and `ureq` imports so this oracle
//! continues to compile unchanged through Phase 3 (`jieba-rs 0.11` inline) and
//! Phase 6 (`ureq 3.5`). Exercises the compiled `asuna-memory` binary directly via
//! CLI, stdio MCP, HTTP Gateway, and inspects SQLite/files on disk.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ffi::c_char;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Once};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const BIN_PATH: &str = env!("CARGO_BIN_EXE_asuna-memory");

static VEC_EXT_INIT: Once = Once::new();

fn register_sqlite_vec() {
    VEC_EXT_INIT.call_once(|| unsafe {
        let func: unsafe extern "C" fn(
            *mut rusqlite::ffi::sqlite3,
            *mut *mut c_char,
            *const rusqlite::ffi::sqlite3_api_routines,
        ) -> i32 = std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
        let rc = rusqlite::ffi::sqlite3_auto_extension(Some(func));
        assert_eq!(
            rc,
            rusqlite::ffi::SQLITE_OK,
            "sqlite3_auto_extension(sqlite3_vec_init) failed with rc={rc}"
        );
    });
}

fn open_db_with_vec(path: &Path) -> Connection {
    register_sqlite_vec();
    Connection::open(path).unwrap_or_else(|e| panic!("open DB {}: {e}", path.display()))
}

fn fixture_path(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(rel)
}

fn sha256_hash8(session_id: &str) -> String {
    let digest = Sha256::digest(session_id.as_bytes());
    digest
        .as_slice()
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn sanitize_cmd(cmd: &mut Command) {
    for key in [
        "AMS_EMBEDDING_API_URL",
        "AMS_EMBEDDING_API_KEY",
        "AMS_EMBEDDING_API_MODEL",
        "AMS_LLM_BASE_URL",
        "AMS_LLM_API_KEY",
        "AMS_LLM_MODEL",
        "AMS_GATEWAY_API_KEY",
        "AMS_GATEWAY_AUTH_ENABLED",
        "AMS_GATEWAY_BIND_HOST",
    ] {
        cmd.env_remove(key);
    }
}

fn write_test_config(data_dir: &Path, embed_api_url: Option<&str>) -> PathBuf {
    let cfg_path = data_dir.join("config.json");
    let mut cfg = json!({
        "data_dir": data_dir,
        "profile_id": "default",
        "graph": { "enabled": true, "remind_on_save": true }
    });
    if let Some(url) = embed_api_url {
        cfg["embedding"] = json!({
            "api_url": url,
            "api_key": "test-key",
            "api_model": "mock-embed-1024",
            "api_format": "openai",
            "dimensions": 1024,
            "batch_size": 16
        });
    }
    std::fs::write(&cfg_path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
    cfg_path
}

fn ensure_profile_layout(data_dir: &Path) -> PathBuf {
    let prof = data_dir.join("profiles").join("default");
    std::fs::create_dir_all(prof.join("conversations")).unwrap();
    std::fs::create_dir_all(prof.join("memory").join("mental_models")).unwrap();
    std::fs::create_dir_all(prof.join("memory").join("intent")).unwrap();
    std::fs::create_dir_all(prof.join("memory").join("scenarios")).unwrap();
    std::fs::create_dir_all(prof.join("refs")).unwrap();
    prof
}

fn run_ams(config_path: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(BIN_PATH);
    sanitize_cmd(&mut cmd);
    cmd.arg("--config")
        .arg(config_path)
        .arg("--profile")
        .arg("default")
        .args(args);
    cmd.output()
        .unwrap_or_else(|e| panic!("failed to spawn {BIN_PATH} {args:?}: {e}"))
}

fn run_ams_ok(config_path: &Path, args: &[&str]) -> String {
    let out = run_ams(config_path, args);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "asuna-memory {args:?} failed (status={:?})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        out.status.code()
    );
    stdout
}

fn run_mcp_calls(config_path: &Path, requests: &[Value]) -> Vec<Value> {
    let mut cmd = Command::new(BIN_PATH);
    sanitize_cmd(&mut cmd);
    cmd.arg("--config")
        .arg(config_path)
        .arg("--profile")
        .arg("default")
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn mcp serve");
    {
        let mut stdin = child.stdin.take().unwrap();
        for req in requests {
            writeln!(stdin, "{}", serde_json::to_string(req).unwrap()).unwrap();
        }
        stdin.flush().unwrap();
    }
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "mcp serve failed (status={:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status.code()
    );
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).expect("valid JSON-RPC line"))
        .collect()
}

struct GatewayHandle {
    addr: SocketAddr,
    child: Child,
    _stdout_thread: Option<JoinHandle<()>>,
    _stderr_thread: Option<JoinHandle<String>>,
}

impl GatewayHandle {
    fn spawn(config_path: &Path) -> Self {
        let mut cmd = Command::new(BIN_PATH);
        sanitize_cmd(&mut cmd);
        cmd.arg("--config")
            .arg(config_path)
            .arg("--profile")
            .arg("default")
            .arg("gateway")
            .arg("--port")
            .arg("0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().expect("spawn gateway");
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let (tx, rx) = mpsc::channel::<SocketAddr>();
        let stdout_thread = std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            let mut sent = false;
            for line in reader.lines().map_while(Result::ok) {
                if !sent {
                    if let Some(rest) = line.split("AMS Gateway listening on http://").nth(1) {
                        if let Ok(addr) = rest.trim().parse::<SocketAddr>() {
                            let _ = tx.send(addr);
                            sent = true;
                        }
                    }
                }
            }
        });
        let stderr_thread = std::thread::spawn(move || {
            let mut buf = String::new();
            let mut reader = BufReader::new(stderr);
            let _ = reader.read_to_string(&mut buf);
            buf
        });

        let addr = match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(a) => a,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let err_log = stderr_thread.join().unwrap_or_default();
                panic!("gateway failed to report listening addr within 15s ({e}):\n{err_log}");
            }
        };

        // Confirm TCP socket accepts connections before returning.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        Self {
            addr,
            child,
            _stdout_thread: Some(stdout_thread),
            _stderr_thread: Some(stderr_thread),
        }
    }

    fn http_request(&self, method: &str, path: &str, json_body: Option<&Value>) -> (u16, Value) {
        let mut stream = TcpStream::connect_timeout(&self.addr, Duration::from_secs(5)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let body_str = json_body.map(|b| serde_json::to_string(b).unwrap());
        let req = match body_str {
            Some(ref b) => format!(
                "{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                self.addr,
                b.len(),
                b
            ),
            None => format!(
                "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                self.addr
            ),
        };
        stream.write_all(req.as_bytes()).unwrap();
        stream.flush().unwrap();

        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        let text = String::from_utf8(raw).expect("UTF-8 HTTP response");
        let (head, body) = text
            .split_once("\r\n\r\n")
            .unwrap_or_else(|| panic!("malformed HTTP response: {text}"));
        let status: u16 = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse().ok())
            .unwrap_or_else(|| panic!("missing HTTP status in: {head}"));
        let parsed: Value = serde_json::from_str(body)
            .unwrap_or_else(|e| panic!("invalid JSON body ({e}): {body}"));
        (status, parsed)
    }

    fn get_json(&self, path: &str) -> (u16, Value) {
        self.http_request("GET", path, None)
    }

    fn post_json(&self, path: &str, body: &Value) -> (u16, Value) {
        self.http_request("POST", path, Some(body))
    }
}

impl Drop for GatewayHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct MockEmbedServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockEmbedServer {
    fn start(vectors_by_text: HashMap<String, Vec<f32>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);

        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_flag.load(Ordering::Acquire) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut content_len = 0usize;
                let mut line = String::new();
                loop {
                    line.clear();
                    let n = reader.read_line(&mut line).unwrap_or(0);
                    if n == 0 || line.trim_end().is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; content_len];
                if content_len > 0 {
                    let _ = reader.read_exact(&mut body);
                }
                let req: Value = serde_json::from_slice(&body).unwrap_or_default();
                let inputs = req
                    .get("input")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();

                let mut data = Vec::with_capacity(inputs.len());
                for (idx, item) in inputs.iter().enumerate() {
                    let text = item.as_str().unwrap_or("");
                    let vec = vectors_by_text.get(text).cloned().unwrap_or_else(|| {
                        panic!("MockEmbedServer received unexpected text: {text:?}")
                    });
                    data.push(json!({ "embedding": vec, "index": idx }));
                }
                let payload = json!({ "data": data }).to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });

        Self {
            addr,
            stop,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }
}

impl Drop for MockEmbedServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn l2_normalize_ref(vec: &mut [f32]) {
    let norm = vec
        .iter()
        .map(|&x| (x as f64) * (x as f64))
        .sum::<f64>()
        .sqrt();
    if norm > 0.0 && norm.is_finite() {
        let inv = 1.0 / norm;
        for x in vec.iter_mut() {
            *x = ((*x as f64) * inv) as f32;
        }
    }
}

fn quantize_to_int8_ref(vec: &[f32]) -> Vec<u8> {
    if vec.is_empty() {
        return Vec::new();
    }
    if vec.iter().any(|v| !v.is_finite()) {
        return vec![0u8; vec.len()];
    }
    let max_abs = vec.iter().fold(0.0f32, |acc, &v| acc.max(v.abs()));
    if max_abs == 0.0 {
        return vec![0u8; vec.len()];
    }
    let scale = 127.0 / f64::from(max_abs);
    vec.iter()
        .map(|&v| ((f64::from(v) * scale).round().clamp(-127.0, 127.0) as i8) as u8)
        .collect()
}

fn decode_f32(v: &Value) -> f32 {
    if let Some(n) = v.as_f64() {
        return n as f32;
    }
    match v.as_str() {
        Some("NaN") => f32::NAN,
        Some("Infinity") => f32::INFINITY,
        Some("-Infinity") => f32::NEG_INFINITY,
        other => panic!("unexpected float token in golden JSON: {other:?}"),
    }
}

fn make_e2e_1024_vectors() -> Vec<(&'static str, &'static str, Vec<f32>, bool)> {
    let v_normal: Vec<f32> = (0..1024)
        .map(|i| (((i % 13) as f32) - 5.0) * 0.1 + 0.03)
        .collect();

    let mut v_neg: Vec<f32> = (0..1024).map(|i| ((i % 11) as f32) * 0.01).collect();
    v_neg[0] = -4.0;

    let mut v_subnormal = vec![0.0f32; 1024];
    v_subnormal[0] = 1e-38f32;
    v_subnormal[1] = -2.5e-39f32;
    v_subnormal[2] = 5e-39f32;

    let v_zero = vec![0.0f32; 1024];

    vec![
        (
            "normal_mixed_1024",
            "golden normal vector turn 1",
            v_normal,
            true,
        ),
        (
            "negative_peak_1024",
            "golden negative peak turn 2",
            v_neg,
            true,
        ),
        (
            "tiny_subnormal_pre_l2_1024",
            "golden subnormal pre-l2 turn 3",
            v_subnormal,
            true,
        ),
        ("all_zero_1024", "golden all zero turn 4", v_zero, false),
    ]
}

fn finalize_single_file_db(conn: Connection, db_path: &Path) {
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode = DELETE;")
        .unwrap();
    drop(conn);
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", db_path.display()));
        if sidecar.exists() {
            std::fs::remove_file(sidecar).unwrap();
        }
    }
}

/// Fixture builder (`#[ignore]`): generates `tests/fixtures/golden/int8_quant_vectors.json`
/// and the three single-file legacy SQLite upgrade databases under `tests/fixtures/v271_upgrade/`.
#[test]
#[ignore]
fn generate_v271_upgrade_fixtures() {
    let v271_dir = fixture_path("v271_upgrade");
    std::fs::create_dir_all(&v271_dir).unwrap();

    // 1. Freeze tests/fixtures/golden/int8_quant_vectors.json
    let direct_specs: Vec<(&str, Value, Vec<f32>)> = vec![
        (
            "normal_proportional",
            json!([0.1, 0.05, -0.025, 0.0]),
            vec![0.1, 0.05, -0.025, 0.0],
        ),
        (
            "subnormal_f64_scale",
            json!([1e-38f64, -2.5e-39f64, 0.0]),
            vec![1e-38f32, -2.5e-39f32, 0.0],
        ),
        (
            "negative_peak",
            json!([0.25, -0.5, -1.0, 0.0]),
            vec![0.25, -0.5, -1.0, 0.0],
        ),
        (
            "all_zero",
            json!([0.0, 0.0, 0.0, 0.0]),
            vec![0.0, 0.0, 0.0, 0.0],
        ),
        (
            "nan_poison",
            json!([1.0, "NaN", 0.5, -0.5]),
            vec![1.0, f32::NAN, 0.5, -0.5],
        ),
        (
            "pos_inf_poison",
            json!([0.5, "Infinity", -0.5, 0.0]),
            vec![0.5, f32::INFINITY, -0.5, 0.0],
        ),
        (
            "neg_inf_poison",
            json!([0.5, "-Infinity", -0.5, 0.0]),
            vec![0.5, f32::NEG_INFINITY, -0.5, 0.0],
        ),
        ("empty", json!([]), vec![]),
    ];
    let direct_cases: Vec<Value> = direct_specs
        .into_iter()
        .map(|(name, raw_input, vals)| {
            let expected: Vec<u8> = quantize_to_int8_ref(&vals);
            json!({ "name": name, "input": raw_input, "expected_u8": expected })
        })
        .collect();

    let e2e_cases: Vec<Value> = make_e2e_1024_vectors()
        .into_iter()
        .map(|(name, text, raw, expect_stored)| {
            let mut normalized = raw.clone();
            l2_normalize_ref(&mut normalized);
            let expected = quantize_to_int8_ref(&normalized);
            json!({
                "name": name,
                "text": text,
                "expect_stored_in_vec_turns": expect_stored,
                "input": raw,
                "expected_u8": expected,
            })
        })
        .collect();

    let quant_path = fixture_path("golden/int8_quant_vectors.json");
    std::fs::create_dir_all(quant_path.parent().unwrap()).unwrap();
    let quant_doc = json!({
        "direct_cases": direct_cases,
        "e2e_1024_cases": e2e_cases,
    });
    std::fs::write(
        &quant_path,
        format!("{}\n", serde_json::to_string_pretty(&quant_doc).unwrap()),
    )
    .unwrap();

    // 2. Build Variant A: tests/fixtures/v271_upgrade/memory.db
    {
        let tmp = TempDir::new().unwrap();
        let prof = ensure_profile_layout(tmp.path());
        let cfg = write_test_config(tmp.path(), None);
        run_ams_ok(&cfg, &["doctor"]);

        let seed_jsonl = tmp.path().join("seed_v271.jsonl");
        let seed_content = concat!(
            r#"{"v":1,"type":"session_header","session_id":"legacy-v271-session","start_time":"2026-04-10T14:30:00.000+08:00","profile_id":"default","source":"gateway","tags":[]}"#,
            "\n",
            r#"{"ts":"2026-04-10T14:30:05.000+08:00","seq":1,"role":"user","content":"我们在升级 asuna-memory 的检索索引"}"#,
            "\n"
        );
        std::fs::write(&seed_jsonl, seed_content).unwrap();
        run_ams_ok(&cfg, &["import", seed_jsonl.to_str().unwrap()]);

        let db_path = prof.join("memory.db");
        {
            let conn = open_db_with_vec(&db_path);
            conn.execute(
                "UPDATE sessions SET start_ts = 1775802600000, file_path = 'gateway://legacy-v271-session' WHERE session_id = 'legacy-v271-session'",
                [],
            )
            .unwrap();
            conn.execute_batch(
                "DROP TRIGGER IF EXISTS bounded_memory_ai;
                 DROP TRIGGER IF EXISTS bounded_memory_ad;
                 DROP TRIGGER IF EXISTS bounded_memory_au;
                 INSERT INTO bounded_memory (id, target, content, created_at, updated_at, confidence, memory_type, supersedes_id)
                 VALUES (1, 'memory', '用户旧偏好：使用 Python 2', 1775802600000, 1775802600000, 'medium', 'atom', NULL);
                 INSERT INTO bounded_memory (id, target, content, created_at, updated_at, confidence, memory_type, supersedes_id)
                 VALUES (2, 'memory', '用户偏好：使用 Rust 与 SQLite', 1775802601000, 1775802601000, 'high', 'atom', 1);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO bounded_memory (id, target, content, created_at, updated_at, confidence, memory_type, supersedes_id)
                 VALUES (3, 'memory', ?1, 1775802602000, 1775802602000, 'medium', 'manual', NULL)",
                params!["第一行旧记录\r\n第二行旧记录"],
            )
            .unwrap();
        }

        // Let doctor recreate triggers and backfill bounded_memory_fts with genuine jieba postings.
        run_ams_ok(&cfg, &["doctor"]);

        {
            let conn = open_db_with_vec(&db_path);
            conn.execute_batch(
                "DROP TRIGGER IF EXISTS turns_ai;
                 DROP TRIGGER IF EXISTS turns_ad;
                 DROP TRIGGER IF EXISTS turns_au;
                 INSERT INTO turns (id, session_id, seq, timestamp_ms, role, preview, char_count)
                 VALUES (2, 'legacy-v271-session', 2, 1775802610000, 'assistant', NULL, 0);
                 UPDATE sessions SET turn_count = 2 WHERE session_id = 'legacy-v271-session';
                 CREATE TRIGGER turns_ai AFTER INSERT ON turns BEGIN
                     INSERT INTO turns_fts(rowid, preview) VALUES (new.id, new.preview);
                 END;
                 CREATE TRIGGER turns_ad AFTER DELETE ON turns BEGIN
                     INSERT INTO turns_fts(turns_fts, rowid, preview) VALUES ('delete', old.id, old.preview);
                 END;
                 CREATE TRIGGER turns_au AFTER UPDATE OF preview ON turns BEGIN
                     INSERT INTO turns_fts(turns_fts, rowid, preview) VALUES ('delete', old.id, old.preview);
                     INSERT INTO turns_fts(rowid, preview) VALUES (new.id, new.preview);
                 END;",
            )
            .unwrap();

            conn.execute("DROP TABLE vec_turns", []).unwrap();
            conn.execute(
                "CREATE VIRTUAL TABLE vec_turns USING vec0(embedding int8[768])",
                [],
            )
            .unwrap();
            let old_int8 = vec![127u8; 768];
            conn.execute(
                "INSERT INTO vec_turns(rowid, embedding) VALUES (1, vec_int8(?1))",
                params![old_int8],
            )
            .unwrap();

            conn.execute("DROP TABLE vec_bounded_memory", []).unwrap();
            if let Err(e) = conn.execute(
                "CREATE VIRTUAL TABLE vec_bounded_memory USING vec0(id INTEGER PRIMARY KEY, embedding float32[768])",
                [],
            ) {
                eprintln!("float32[768] DDL rejected ({e}); falling back to float[768]");
                conn.execute(
                    "CREATE VIRTUAL TABLE vec_bounded_memory USING vec0(id INTEGER PRIMARY KEY, embedding float[768])",
                    [],
                )
                .unwrap();
            } else {
                eprintln!("float32[768] DDL accepted by sqlite-vec");
            }
            let mut old_f32_blob = Vec::with_capacity(768 * 4);
            for _ in 0..768 {
                old_f32_blob.extend_from_slice(&1.0f32.to_le_bytes());
            }
            conn.execute(
                "INSERT INTO vec_bounded_memory(id, embedding) VALUES (2, vec_f32(?1))",
                params![old_f32_blob],
            )
            .unwrap();

            conn.execute_batch("DROP TABLE IF EXISTS capture_dedup; PRAGMA user_version = 3;")
                .unwrap();
            finalize_single_file_db(conn, &db_path);
        }
        std::fs::copy(&db_path, v271_dir.join("memory.db")).unwrap();
    }

    // 3. Build Variant B1: tests/fixtures/v271_upgrade/variant_b_both_unicode61.db
    {
        let tmp = TempDir::new().unwrap();
        let prof = ensure_profile_layout(tmp.path());
        let cfg = write_test_config(tmp.path(), None);
        run_ams_ok(&cfg, &["doctor"]);

        let db_path = prof.join("memory.db");
        let conn = open_db_with_vec(&db_path);
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS turns_ai;
             DROP TRIGGER IF EXISTS turns_ad;
             DROP TRIGGER IF EXISTS turns_au;
             DROP TABLE IF EXISTS turns_fts;
             DROP TRIGGER IF EXISTS bounded_memory_ai;
             DROP TRIGGER IF EXISTS bounded_memory_ad;
             DROP TRIGGER IF EXISTS bounded_memory_au;
             DROP TABLE IF EXISTS bounded_memory_fts;

             CREATE VIRTUAL TABLE turns_fts USING fts5(preview, content='', content_rowid=id, tokenize='unicode61 remove_diacritics 2');
             CREATE VIRTUAL TABLE bounded_memory_fts USING fts5(content, content='bounded_memory', content_rowid='id', tokenize='unicode61 remove_diacritics 2');

             INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at, turn_count)
             VALUES ('legacy-u61-session', 1775802600000, 'gateway://legacy-u61', 1775802600000, 1775802600000, 2);
             INSERT INTO turns (id, session_id, seq, timestamp_ms, role, preview, char_count)
             VALUES (1, 'legacy-u61-session', 1, 1775802605000, 'user', '神经网络与向量检索架构', 11);
             INSERT INTO turns (id, session_id, seq, timestamp_ms, role, preview, char_count)
             VALUES (2, 'legacy-u61-session', 2, 1775802610000, 'assistant', 'rust compiler lifetime', 22);
             INSERT INTO turns_fts(rowid, preview) VALUES (1, '神 经 网 络 与 向 量 检 索 架 构');
             INSERT INTO turns_fts(rowid, preview) VALUES (2, 'rust compiler lifetime');

             INSERT INTO bounded_memory (id, target, content, created_at, updated_at, confidence, memory_type)
             VALUES (1, 'memory', '用户喜欢中文分词检索', 1775802600000, 1775802600000, 'high', 'atom');
             INSERT INTO bounded_memory (id, target, content, created_at, updated_at, confidence, memory_type)
             VALUES (2, 'memory', 'Rust async runtime', 1775802601000, 1775802601000, 'medium', 'manual');
             INSERT INTO bounded_memory_fts(rowid, content) VALUES (1, '用 户 喜 欢 中 文 分 词 检 索');
             INSERT INTO bounded_memory_fts(rowid, content) VALUES (2, 'Rust async runtime');

             CREATE TRIGGER turns_ai AFTER INSERT ON turns BEGIN INSERT INTO turns_fts(rowid, preview) VALUES (new.id, tokenize_zh(new.preview)); END;
             CREATE TRIGGER turns_ad AFTER DELETE ON turns BEGIN INSERT INTO turns_fts(turns_fts, rowid, preview) VALUES ('delete', old.id, tokenize_zh(old.preview)); END;
             CREATE TRIGGER turns_au AFTER UPDATE ON turns BEGIN INSERT INTO turns_fts(turns_fts, rowid, preview) VALUES ('delete', old.id, tokenize_zh(old.preview)); INSERT INTO turns_fts(rowid, preview) VALUES (new.id, tokenize_zh(new.preview)); END;
             CREATE TRIGGER bounded_memory_ai AFTER INSERT ON bounded_memory BEGIN INSERT INTO bounded_memory_fts(rowid, content) VALUES (new.id, tokenize_zh(new.content)); END;

             DROP TABLE IF EXISTS capture_dedup;
             PRAGMA user_version = 0;",
        )
        .unwrap();
        finalize_single_file_db(conn, &db_path);
        std::fs::copy(&db_path, v271_dir.join("variant_b_both_unicode61.db")).unwrap();
    }

    // 4. Build Variant B2: tests/fixtures/v271_upgrade/variant_b_bm_only_unicode61.db
    {
        let tmp = TempDir::new().unwrap();
        let prof = ensure_profile_layout(tmp.path());
        let cfg = write_test_config(tmp.path(), None);
        run_ams_ok(&cfg, &["doctor"]);

        let seed_jsonl = tmp.path().join("seed_u61_b2.jsonl");
        let seed_content = concat!(
            r#"{"v":1,"type":"session_header","session_id":"legacy-bm-only-u61","start_time":"2026-04-10T14:30:00.000+08:00","profile_id":"default","source":"cli","tags":[]}"#,
            "\n",
            r#"{"ts":"2026-04-10T14:30:05.000+08:00","seq":1,"role":"user","content":"正常 jieba 对话轮次"}"#,
            "\n"
        );
        std::fs::write(&seed_jsonl, seed_content).unwrap();
        run_ams_ok(&cfg, &["import", seed_jsonl.to_str().unwrap()]);

        let db_path = prof.join("memory.db");
        let conn = open_db_with_vec(&db_path);
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS bounded_memory_ai;
             DROP TRIGGER IF EXISTS bounded_memory_ad;
             DROP TRIGGER IF EXISTS bounded_memory_au;
             DROP TABLE IF EXISTS bounded_memory_fts;

             CREATE VIRTUAL TABLE bounded_memory_fts USING fts5(content, content='bounded_memory', content_rowid='id', tokenize='unicode61 remove_diacritics 2');
             INSERT INTO bounded_memory (id, target, content, created_at, updated_at, confidence, memory_type)
             VALUES (1, 'memory', '用户偏好混合语义检索', 1775802600000, 1775802600000, 'high', 'atom');
             INSERT INTO bounded_memory_fts(rowid, content) VALUES (1, '用 户 偏 好 混 合 语 义 检 索');
             CREATE TRIGGER bounded_memory_ai AFTER INSERT ON bounded_memory BEGIN INSERT INTO bounded_memory_fts(rowid, content) VALUES (new.id, tokenize_zh(new.content)); END;",
        )
        .unwrap();
        let ver: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, 5, "Variant B2 fixture must retain user_version=5");
        finalize_single_file_db(conn, &db_path);
        std::fs::copy(&db_path, v271_dir.join("variant_b_bm_only_unicode61.db")).unwrap();
    }
}

#[test]
fn test_v271_variant_a_upgrade_migrations_and_legacy_cleanup() {
    let tmp = TempDir::new().unwrap();
    let prof = ensure_profile_layout(tmp.path());
    let cfg = write_test_config(tmp.path(), None);

    let db_path = prof.join("memory.db");
    std::fs::copy(fixture_path("v271_upgrade/memory.db"), &db_path).unwrap();
    let legacy_rel = Path::new("2026/04/10/20260410T143000_legacy-v.jsonl");
    let legacy_dest = prof.join("conversations").join(legacy_rel);
    std::fs::create_dir_all(legacy_dest.parent().unwrap()).unwrap();
    std::fs::copy(
        fixture_path("v271_upgrade/conversations/2026/04/10/20260410T143000_legacy-v.jsonl"),
        &legacy_dest,
    )
    .unwrap();

    // Verify frozen v2.7.1 Variant A preconditions before running binary.
    let null_turn_id: i64 = {
        let conn = open_db_with_vec(&db_path);
        let ver: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, 3);
        let has_dedup: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='capture_dedup')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!has_dedup);
        let turns_ai: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='trigger' AND name='turns_ai'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!turns_ai.contains("IS NOT NULL"));
        let vt_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='vec_turns'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(vt_sql.contains("int8[768]") && !vt_sql.contains("cosine"));
        let vbm_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='vec_bounded_memory'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(vbm_sql.contains("768") && !vbm_sql.contains("cosine"));
        conn.query_row(
            "SELECT id FROM turns WHERE session_id='legacy-v271-session' AND preview IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };

    // 1. Auto-upgrade on first writable start (`doctor --verbose`).
    let doc_out = run_ams_ok(&cfg, &["doctor", "--verbose"]);
    assert!(
        doc_out.contains("完整性检查: OK"),
        "doctor output:\n{doc_out}"
    );

    {
        let conn = open_db_with_vec(&db_path);
        let ver: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert!(
            ver >= 5,
            "expected user_version >= 5 after upgrade, got {ver}"
        );

        let has_dedup: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='capture_dedup')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(has_dedup, "capture_dedup must exist after upgrade");

        let trig_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN \
                 ('turns_ai','turns_ad','turns_au','bounded_memory_ai','bounded_memory_ad','bounded_memory_au')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(trig_count, 6);
        for tname in ["turns_ai", "turns_ad"] {
            let tsql: String = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type='trigger' AND name=?1",
                    params![tname],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                tsql.contains("IS NOT NULL"),
                "{tname} missing NULL guard: {tsql}"
            );
        }

        for tbl in ["vec_turns", "vec_bounded_memory"] {
            let sql: String = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE name=?1",
                    params![tbl],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                sql.contains("int8[1024]") && sql.contains("distance_metric=cosine"),
                "{tbl} must migrate to int8[1024] cosine, got: {sql}"
            );
        }

        let docsize: i64 = conn
            .query_row("SELECT COUNT(*) FROM turns_fts_docsize", [], |r| r.get(0))
            .unwrap();
        assert_eq!(docsize, 1, "only the non-NULL turn must be in turns_fts");
    }

    // 2. D2-7 guard: deleting the NULL-preview turn must succeed without FTS5_CORRUPT.
    let del_out = run_ams_ok(&cfg, &["delete-turn", &null_turn_id.to_string()]);
    assert!(del_out.contains(&format!("Deleted turn {null_turn_id}")));

    // 3. Reconcile bounded_memory + verify CRLF tolerance & superseded exclusion.
    run_ams_ok(&cfg, &["doctor", "--fix"]);
    let memory_md = std::fs::read_to_string(prof.join("memory").join("MEMORY.md")).unwrap();
    assert!(memory_md.contains("用户偏好：使用 Rust 与 SQLite"));
    assert!(memory_md.contains("第一行旧记录"));
    assert!(memory_md.contains("第二行旧记录"));
    assert!(
        !memory_md.contains("使用 Python 2"),
        "superseded atom must never be exported into MEMORY.md: {memory_md}"
    );
    let check_out = run_ams_ok(&cfg, &["doctor"]);
    assert!(
        check_out.contains("bounded_memory[memory]: OK (2 entries)"),
        "doctor after fix should converge (no CRLF divergence loop):\n{check_out}"
    );

    // 4. MCP save_session overwrites & removes pre-U5 gateway legacy file.
    let save_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "save_session",
            "arguments": {
                "session_id": "legacy-v271-session",
                "turns": [
                    {
                        "timestamp": "2026-04-10T14:30:00.000+08:00",
                        "role": "user",
                        "content": "我们在升级 asuna-memory 的检索索引"
                    },
                    {
                        "timestamp": "2026-04-10T14:30:10.000+08:00",
                        "role": "assistant",
                        "content": "已迁移至 canonical hash8 文件名"
                    }
                ]
            }
        }
    });
    let resps = run_mcp_calls(&cfg, &[save_req]);
    assert_eq!(resps.len(), 1);
    assert!(
        resps[0]["result"]["isError"].is_null(),
        "resp: {}",
        resps[0]
    );

    assert!(
        !legacy_dest.exists(),
        "pre-U5 legacy JSONL should be removed once canonical save_session succeeds"
    );
    let hash8 = sha256_hash8("legacy-v271-session");
    let canonical_dest = prof
        .join("conversations")
        .join("2026/04/10")
        .join(format!("20260410T143000_{hash8}.jsonl"));
    assert!(
        canonical_dest.exists(),
        "canonical JSONL must exist at {}",
        canonical_dest.display()
    );

    let conn = open_db_with_vec(&db_path);
    let stored_fp: String = conn
        .query_row(
            "SELECT file_path FROM sessions WHERE session_id='legacy-v271-session'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!stored_fp.starts_with("gateway://"), "got {stored_fp}");
}

#[test]
fn test_v271_variant_b_both_unicode61_migrates_to_jieba() {
    let tmp = TempDir::new().unwrap();
    let prof = ensure_profile_layout(tmp.path());
    let cfg = write_test_config(tmp.path(), None);

    let db_path = prof.join("memory.db");
    std::fs::copy(
        fixture_path("v271_upgrade/variant_b_both_unicode61.db"),
        &db_path,
    )
    .unwrap();

    {
        let conn = open_db_with_vec(&db_path);
        let ver: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, 0);
        for tbl in ["turns_fts", "bounded_memory_fts"] {
            let sql: String = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                    params![tbl],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(sql.contains("unicode61"), "{tbl} precondition: {sql}");
        }
    }

    run_ams_ok(&cfg, &["doctor"]);

    {
        let conn = open_db_with_vec(&db_path);
        let ver: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert!(ver >= 5, "expected user_version >= 5, got {ver}");
        for tbl in ["turns_fts", "bounded_memory_fts"] {
            let sql: String = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                    params![tbl],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                sql.contains("tokenize='jieba'") && !sql.contains("unicode61"),
                "{tbl} must migrate to jieba, got: {sql}"
            );
        }
        let turns_ai: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='trigger' AND name='turns_ai'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!turns_ai.contains("tokenize_zh"), "{turns_ai}");
        let bm_docsize: i64 = conn
            .query_row("SELECT COUNT(*) FROM bounded_memory_fts_docsize", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            bm_docsize, 2,
            "both bounded_memory rows must be indexed after jieba migration"
        );
    }

    let search_out = run_ams_ok(&cfg, &["search", "向量检索", "--mode", "keyword"]);
    assert!(
        search_out.contains("神经网络与向量检索架构"),
        "keyword search after unicode61->jieba migration failed:\n{search_out}"
    );

    let gw = GatewayHandle::spawn(&cfg);
    let (st, recall) = gw.post_json("/recall", &json!({"query": "中文分词", "top_k": 5}));
    assert_eq!(st, 200, "recall response: {recall}");
    let mems = recall["memories"].as_array().unwrap();
    assert!(
        mems.iter()
            .any(|m| m["layer"] == "L1" && m["content"] == "用户喜欢中文分词检索"),
        "L1 jieba FTS recall missing migrated Chinese row: {mems:?}"
    );
    assert!(
        !mems.iter().any(|m| m["content"] == "Rust async runtime"),
        "unrelated English row must not match Chinese query: {mems:?}"
    );
}

#[test]
fn test_v271_variant_b_bm_only_unicode61_d28_migrates() {
    let tmp = TempDir::new().unwrap();
    let prof = ensure_profile_layout(tmp.path());
    let cfg = write_test_config(tmp.path(), None);

    let db_path = prof.join("memory.db");
    std::fs::copy(
        fixture_path("v271_upgrade/variant_b_bm_only_unicode61.db"),
        &db_path,
    )
    .unwrap();

    {
        let conn = open_db_with_vec(&db_path);
        let ver: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, 5);
        let turns_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='turns_fts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(turns_sql.contains("jieba"), "{turns_sql}");
        let bm_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='bounded_memory_fts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(bm_sql.contains("unicode61"), "{bm_sql}");
    }

    run_ams_ok(&cfg, &["doctor"]);

    {
        let conn = open_db_with_vec(&db_path);
        let bm_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='bounded_memory_fts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            bm_sql.contains("tokenize='jieba'") && !bm_sql.contains("unicode61"),
            "D2-8 failed to migrate isolated bounded_memory_fts: {bm_sql}"
        );
        let bm_docsize: i64 = conn
            .query_row("SELECT COUNT(*) FROM bounded_memory_fts_docsize", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            bm_docsize, 1,
            "isolated bounded_memory row must be indexed after D2-8 migration"
        );
    }

    let gw = GatewayHandle::spawn(&cfg);
    let (st, recall) = gw.post_json("/recall", &json!({"query": "语义检索", "top_k": 5}));
    assert_eq!(st, 200, "recall response: {recall}");
    let mems = recall["memories"].as_array().unwrap();
    assert!(
        mems.iter()
            .any(|m| m["layer"] == "L1" && m["content"] == "用户偏好混合语义检索"),
        "L1 jieba FTS recall missing D2-8 migrated row: {mems:?}"
    );
}

#[test]
fn test_golden_jsonl_canonical_and_legacy_contracts() {
    let hash8 = sha256_hash8("golden-session-v3");
    assert_eq!(hash8, "9be71fb1");
    let canonical_rel = format!("conversations/2026/04/10/20260410T143000_{hash8}.jsonl");
    let golden_canonical = fixture_path(&format!("golden/{canonical_rel}"));
    let golden_bytes = std::fs::read(&golden_canonical).unwrap();

    // Import canonical JSONL into fresh profile and assert byte-for-byte identical output.
    {
        let tmp = TempDir::new().unwrap();
        let prof = ensure_profile_layout(tmp.path());
        let cfg = write_test_config(tmp.path(), None);
        run_ams_ok(&cfg, &["import", golden_canonical.to_str().unwrap()]);
        let imported_path = prof.join(&canonical_rel);
        let imported_bytes = std::fs::read(&imported_path)
            .unwrap_or_else(|e| panic!("missing {}: {e}", imported_path.display()));
        assert_eq!(
            imported_bytes, golden_bytes,
            "canonical JSONL bytes drifted from golden contract"
        );
    }

    // Legacy pre-U5 JSONL must rebuild cleanly and export all turns.
    {
        let tmp = TempDir::new().unwrap();
        let prof = ensure_profile_layout(tmp.path());
        let cfg = write_test_config(tmp.path(), None);
        let legacy_rel = "2026/04/10/20260410T143000_golden-l.jsonl";
        let dest = prof.join("conversations").join(legacy_rel);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(
            fixture_path(&format!("golden/conversations/{legacy_rel}")),
            &dest,
        )
        .unwrap();

        let rebuild_out = run_ams_ok(&cfg, &["rebuild", "--full"]);
        assert!(rebuild_out.contains("1 个会话, 2 轮对话"), "{rebuild_out}");

        let export_out = run_ams_ok(&cfg, &["export", "golden-legacy-session"]);
        assert!(
            export_out.contains("旧命名文件重建兼容性测试"),
            "{export_out}"
        );
        assert!(
            export_out.contains("pre-U5 文件可被 rebuild 正常索引"),
            "{export_out}"
        );
    }
}

#[test]
fn test_golden_bounded_memory_md_format_and_reconcile() {
    let tmp = TempDir::new().unwrap();
    let prof = ensure_profile_layout(tmp.path());
    let cfg = write_test_config(tmp.path(), None);

    let golden_mem = std::fs::read_to_string(fixture_path("golden/memory/MEMORY.md")).unwrap();
    let golden_user = std::fs::read_to_string(fixture_path("golden/memory/USER.md")).unwrap();
    let mem_path = prof.join("memory").join("MEMORY.md");
    let user_path = prof.join("memory").join("USER.md");
    std::fs::write(&mem_path, &golden_mem).unwrap();
    std::fs::write(&user_path, &golden_user).unwrap();

    run_ams_ok(&cfg, &["doctor", "--fix"]);

    let conn = open_db_with_vec(&prof.join("memory.db"));
    let mem_rows: Vec<String> = {
        let mut s = conn
            .prepare("SELECT content FROM bounded_memory WHERE target='memory' ORDER BY id")
            .unwrap();
        s.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(
        mem_rows,
        vec![
            "1. 偏好 Rust 工具链".to_string(),
            "2. 参考 §5 文档规范".to_string()
        ]
    );
    let user_rows: Vec<String> = {
        let mut s = conn
            .prepare("SELECT content FROM bounded_memory WHERE target='user' ORDER BY id")
            .unwrap();
        s.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(
        user_rows,
        vec![
            "1. 资深系统工程师".to_string(),
            "2. 常用中文交流".to_string()
        ]
    );

    // Check header contract & byte-for-byte identical body.
    for (path, golden, prefix) in [
        (
            &mem_path,
            &golden_mem,
            "<!-- ASUNA MEMORY | capacity: 2200 chars | updated: ",
        ),
        (
            &user_path,
            &golden_user,
            "<!-- ASUNA USER PROFILE | capacity: 1375 chars | updated: ",
        ),
    ] {
        let rendered = std::fs::read_to_string(path).unwrap();
        assert!(
            !rendered.contains('\r'),
            "must stay LF-only: {}",
            path.display()
        );
        let (r_head, r_body) = rendered.split_once("\n\n").unwrap();
        let (_g_head, g_body) = golden.split_once("\n\n").unwrap();
        assert!(
            r_head.starts_with(prefix) && r_head.ends_with(" -->"),
            "bad header: {r_head}"
        );
        assert_eq!(
            r_body.as_bytes(),
            g_body.as_bytes(),
            "body of {} drifted from golden fixture",
            path.display()
        );
    }

    let doc_out = run_ams_ok(&cfg, &["doctor"]);
    assert!(
        doc_out.contains("bounded_memory[memory]: OK (2 entries)"),
        "{doc_out}"
    );
    assert!(
        doc_out.contains("bounded_memory[user]: OK (2 entries)"),
        "{doc_out}"
    );
}

#[test]
fn test_golden_l2_l3_l4_l5_and_refs_contracts() {
    // 1. Validate golden L2 scenario YAML frontmatter + file naming contract.
    let scenario_name = "1775802600000_101.md";
    let scenario_text = std::fs::read_to_string(fixture_path(&format!(
        "golden/memory/scenarios/{scenario_name}"
    )))
    .unwrap();
    assert!(!scenario_text.contains('\r'));
    assert!(scenario_text.starts_with("---\n"));
    assert!(scenario_text.contains("\ntitle: \"Rust: 异步运行时调试\"\n"));
    assert!(scenario_text.contains("\natom_ids: [10, 11]\n"));
    assert!(scenario_text.contains("\ncreated_at: 1775802600000\n"));
    assert!(scenario_text.contains("\nupdated_at: 1775802600000\n---\n\n"));

    // 2. Seed a fresh profile with golden L3 persona, stale L4/L5 docs, and refs/.
    let tmp = TempDir::new().unwrap();
    let prof = ensure_profile_layout(tmp.path());
    let cfg = write_test_config(tmp.path(), None);

    let golden_persona = std::fs::read_to_string(fixture_path("golden/memory/persona.md")).unwrap();
    std::fs::write(prof.join("memory").join("persona.md"), &golden_persona).unwrap();

    let l4_l5_rels = [
        "memory/mental_models/workflow-patterns.md",
        "memory/mental_models/decision-framework.md",
        "memory/mental_models/communication-style.md",
        "memory/intent/likely-next-topics.md",
        "memory/intent/anticipated-needs.md",
    ];
    for rel in l4_l5_rels {
        let src = fixture_path(&format!("golden/{rel}"));
        let dst = prof.join(rel);
        std::fs::copy(&src, &dst).unwrap();
    }

    let golden_ref = std::fs::read_to_string(fixture_path("golden/refs/task-1/step_1.md")).unwrap();
    let ref_dest = prof.join("refs").join("task-1").join("step_1.md");
    std::fs::create_dir_all(ref_dest.parent().unwrap()).unwrap();
    std::fs::write(&ref_dest, &golden_ref).unwrap();

    // First gateway launch: verify /persona, stale-L4/L5 gate, GET /recall/task-1/step_1, and POST /offload.
    {
        let gw = GatewayHandle::spawn(&cfg);
        let (st, p_resp) = gw.get_json("/persona");
        assert_eq!(st, 200);
        assert_eq!(p_resp["status"], "ok");
        assert_eq!(p_resp["source"], "persona.md");
        assert_eq!(p_resp["persona"].as_str().unwrap(), golden_persona);

        let (st, r_resp) = gw.post_json("/recall", &json!({"query": "工作流", "top_k": 10}));
        assert_eq!(st, 200);
        let mems = r_resp["memories"].as_array().unwrap();
        assert!(
            mems.iter().any(|m| m["layer"] == "L3"),
            "L3 persona must surface"
        );
        assert!(
            !mems.iter().any(|m| m["layer"] == "L4" || m["layer"] == "L5"),
            "stale golden L4/L5 docs from 2026-04-10 must be filtered by 7d freshness gate: {mems:?}"
        );

        let (st, node_resp) = gw.get_json("/recall/task-1/step_1");
        assert_eq!(st, 200);
        assert_eq!(node_resp["node_id"], "task-1/step_1");
        assert_eq!(node_resp["content"].as_str().unwrap(), golden_ref);

        let (st, off_resp) = gw.post_json(
            "/offload",
            &json!({"task_id": "task-2", "content": golden_ref}),
        );
        assert_eq!(st, 200);
        assert_eq!(off_resp["node_id"], "task-2/step_1");
        let offloaded_bytes = std::fs::read(prof.join("refs/task-2/step_1.md")).unwrap();
        assert_eq!(offloaded_bytes, golden_ref.as_bytes());
    }

    // Stamp current UTC `Updated:` on temp L4/L5 copies and verify all 5 surface in /recall.
    let now_utc = chrono::Utc::now()
        .format("%Y-%m-%d %H:%M:%S UTC")
        .to_string();
    for rel in l4_l5_rels {
        let p = prof.join(rel);
        let text = std::fs::read_to_string(&p).unwrap();
        let updated = text.replacen("2026-04-10 06:30:00 UTC", &now_utc, 1);
        std::fs::write(&p, updated).unwrap();
    }

    {
        let gw = GatewayHandle::spawn(&cfg);
        let (st, r_resp) = gw.post_json("/recall", &json!({"query": "工作流", "top_k": 10}));
        assert_eq!(st, 200);
        let mems = r_resp["memories"].as_array().unwrap();
        let l4_count = mems.iter().filter(|m| m["layer"] == "L4").count();
        let l5_count = mems.iter().filter(|m| m["layer"] == "L5").count();
        assert_eq!(l4_count, 3, "all 3 fresh L4 docs must surface: {mems:?}");
        assert_eq!(l5_count, 2, "all 2 fresh L5 docs must surface: {mems:?}");
        let ctx = r_resp["context"].as_str().unwrap();
        assert!(ctx.contains("先写回归测试再修改核心路径"));
        assert!(ctx.contains("提供新旧二进制差分回归证明"));
    }
}

#[test]
fn test_golden_int8_quant_vectors_e2e_and_contract() {
    let raw = std::fs::read_to_string(fixture_path("golden/int8_quant_vectors.json")).unwrap();
    let doc: Value = serde_json::from_str(&raw).unwrap();

    // 1. Validate direct edge cases against reference f64 absmax contract.
    let direct_cases = doc["direct_cases"].as_array().unwrap();
    assert_eq!(direct_cases.len(), 8);
    for c in direct_cases {
        let name = c["name"].as_str().unwrap();
        let input: Vec<f32> = c["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(decode_f32)
            .collect();
        let expected: Vec<u8> = c["expected_u8"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| u8::try_from(n.as_u64().unwrap()).unwrap())
            .collect();
        assert_eq!(quantize_to_int8_ref(&input), expected, "direct case {name}");
    }

    // 2. Drive full pipeline: MockEmbedServer -> `asuna-memory import` -> SQLite `vec_turns`.
    let e2e_cases = doc["e2e_1024_cases"].as_array().unwrap();
    assert_eq!(e2e_cases.len(), 4);

    let mut mock_map = HashMap::new();
    let mut turns = Vec::new();
    let mut expected_rows: Vec<(u32, String, bool, Vec<u8>)> = Vec::new();
    for (idx, c) in e2e_cases.iter().enumerate() {
        let seq = (idx + 1) as u32;
        let text = c["text"].as_str().unwrap().to_string();
        let expect_stored = c["expect_stored_in_vec_turns"].as_bool().unwrap();
        let input: Vec<f32> = c["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(decode_f32)
            .collect();
        let expected_u8: Vec<u8> = c["expected_u8"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| u8::try_from(n.as_u64().unwrap()).unwrap())
            .collect();
        assert_eq!(input.len(), 1024);
        assert_eq!(expected_u8.len(), 1024);
        mock_map.insert(text.clone(), input);
        turns.push(json!({
            "ts": format!("2026-04-10T14:30:0{seq}.000+08:00"),
            "seq": seq,
            "role": "user",
            "content": text,
        }));
        expected_rows.push((
            seq,
            c["name"].as_str().unwrap().to_string(),
            expect_stored,
            expected_u8,
        ));
    }

    let mock = MockEmbedServer::start(mock_map);
    let tmp = TempDir::new().unwrap();
    let prof = ensure_profile_layout(tmp.path());
    let cfg = write_test_config(tmp.path(), Some(&mock.base_url()));

    let jsonl_path = tmp.path().join("quant_e2e.jsonl");
    let mut lines = String::new();
    lines.push_str(
        r#"{"v":1,"type":"session_header","session_id":"quant-golden-e2e","start_time":"2026-04-10T14:30:00.000+08:00","profile_id":"default","source":"e2e","tags":[]}"#,
    );
    lines.push('\n');
    for t in &turns {
        lines.push_str(&serde_json::to_string(t).unwrap());
        lines.push('\n');
    }
    std::fs::write(&jsonl_path, lines).unwrap();

    let import_out = run_ams_ok(&cfg, &["import", jsonl_path.to_str().unwrap()]);
    assert!(import_out.contains("4 轮"), "{import_out}");

    let conn = open_db_with_vec(&prof.join("memory.db"));
    let turn_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(turn_count, 4);
    let vec_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM vec_turns_rowids", [], |r| r.get(0))
        .unwrap();
    assert_eq!(vec_count, 3, "zero vector turn 4 must be skipped per W13");

    for (seq, name, expect_stored, expected_u8) in expected_rows {
        let turn_id: i64 = conn
            .query_row(
                "SELECT id FROM turns WHERE session_id='quant-golden-e2e' AND seq=?1",
                params![seq as i64],
                |r| r.get(0),
            )
            .unwrap();
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT embedding FROM vec_turns WHERE rowid=?1",
                params![turn_id],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        if expect_stored {
            let actual = blob.unwrap_or_else(|| panic!("missing vec_turns row for {name}"));
            assert_eq!(
                actual, expected_u8,
                "vec_turns stored int8 bytes mismatched golden case `{name}`"
            );
        } else {
            assert!(
                blob.is_none(),
                "case `{name}` must NOT be inserted in vec_turns"
            );
        }
    }
}
