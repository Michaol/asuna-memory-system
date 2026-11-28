//! STEP 6 cross-process smoke: the backfill cost guard (`backfill.lock`) must let
//! exactly ONE process pay the embedding cost when a gateway and an MCP `serve`
//! run against the SAME profile. Uses the library `Db` for seeding/inspection (it
//! registers the in-process jieba FTS5 tokenizer + sqlite-vec, so direct atom INSERTs
//! and vec reads work), and spawns the real binary via `CARGO_BIN_EXE_asuna-memory`
//! for the cross-process part. A counting offline mock embedder returns valid
//! 1024-dim vectors so the holder's backfill succeeds.

use asuna_memory::index::db::Db;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_asuna-memory");
const DIM: usize = 1024;
/// Poll cadence while waiting for cross-process backfill convergence.
const CONVERGENCE_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Number of live atoms seeded for the backfill to embed.
const SEEDED_ATOM_COUNT: i64 = 3;

/// Offline OpenAI-compatible embedder: answers `/v1/embeddings` with one valid
/// axis unit vector per input and counts requests.
struct MockEmbedder {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockEmbedder {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let cnt = Arc::clone(&hits);
        let flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if flag.load(Ordering::Acquire) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                cnt.fetch_add(1, Ordering::AcqRel);
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut len = 0usize;
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim_end().is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; len];
                let _ = reader.read_exact(&mut body);
                let req: Value = serde_json::from_slice(&body).unwrap_or_default();
                let n = match req.get("input") {
                    Some(Value::Array(a)) => a.len(),
                    Some(Value::String(_)) => 1,
                    _ => 0,
                };
                let mut vec0 = vec![0.0f32; DIM];
                vec0[0] = 1.0;
                let data: Vec<Value> = (0..n)
                    .map(|i| json!({"embedding": vec0, "index": i}))
                    .collect();
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
            hits,
            stop,
            handle: Some(handle),
        }
    }

    fn embed_config(&self) -> Value {
        json!({
            "api_url": format!("http://{}/v1", self.addr),
            "api_key": "mock-key",
            "api_model": "mock-embed-1024",
            "api_format": "openai",
            "dimensions": DIM,
            "batch_size": 16
        })
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::Acquire)
    }
}

impl Drop for MockEmbedder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// A spawned binary whose stdout/stderr are drained (so pipe buffers never block
/// the child) and whose stdin can be held open (MCP `serve` exits on stdin EOF).
struct Proc {
    child: Child,
    _stdin: Option<ChildStdin>,
    drains: Vec<JoinHandle<()>>,
}

impl Proc {
    fn spawn(args: &[&str], cfg: &Path, hold_stdin: bool) -> Self {
        let mut cmd = Command::new(BIN);
        cmd.arg("--config")
            .arg(cfg)
            .arg("--profile")
            .arg("default")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.stdin(if hold_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut child = cmd.spawn().expect("spawn binary");
        let stdin = child.stdin.take();
        let mut drains = Vec::new();
        if let Some(out) = child.stdout.take() {
            drains.push(std::thread::spawn(move || {
                let mut buf = String::new();
                let _ = BufReader::new(out).read_to_string(&mut buf);
            }));
        }
        if let Some(err) = child.stderr.take() {
            drains.push(std::thread::spawn(move || {
                let mut buf = String::new();
                let _ = BufReader::new(err).read_to_string(&mut buf);
            }));
        }
        Self {
            child,
            _stdin: stdin,
            drains,
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for h in self.drains.drain(..) {
            let _ = h.join();
        }
    }
}

fn write_config(data_dir: &Path, embedding: Value) -> PathBuf {
    let cfg = data_dir.join("config.json");
    let body = json!({
        "data_dir": data_dir,
        "profile_id": "default",
        "graph": { "enabled": false },
        "gateway": { "auth_enabled": false, "bind_host": "127.0.0.1" },
        "embedding": embedding,
    });
    std::fs::write(&cfg, serde_json::to_string_pretty(&body).unwrap()).unwrap();
    cfg
}

/// (vec_count, live_atom_count, distinct_vec_ids) over a single inspection
/// connection. Each query is autocommit, so in WAL mode every poll starts a fresh
/// read transaction and sees the writer processes' latest commits — no need to
/// reopen the Db per inspection.
fn inspect_counts(db: &Db) -> (i64, i64, i64) {
    let vecs: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM vec_bounded_memory", [], |r| r.get(0))
        .unwrap();
    let distinct: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(DISTINCT id) FROM vec_bounded_memory",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let atoms: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM bounded_memory bm \
             WHERE COALESCE(bm.memory_type,'manual')='atom' AND bm.content IS NOT NULL \
               AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id=bm.id)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    (vecs, atoms, distinct)
}

/// Open ONE inspection connection and poll until the seeded atoms are fully
/// backfilled (vec_count == live_atom_count == SEEDED_ATOM_COUNT, no duplicate vec
/// ids), or fail at the deadline.
fn wait_for_backfill_convergence(db_path: &Path, label: &str) {
    let db = Db::open(db_path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (vecs, atoms, distinct) = inspect_counts(&db);
        if vecs == atoms && atoms == SEEDED_ATOM_COUNT && distinct == vecs {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{label}: backfill did not converge (vecs={vecs}, atoms={atoms}, distinct={distinct})"
        );
        std::thread::sleep(CONVERGENCE_POLL_INTERVAL);
    }
}

/// Wipe the vec index so the next process must re-embed the seeded atoms. Re-assert
/// user_version=6 (the current schema marker) so the restarted process sees a
/// current schema. Uses the library Db so the vec0 table is registered.
fn wipe_vec_index(db_path: &Path) {
    let db = Db::open(db_path).unwrap();
    db.conn()
        .execute_batch("PRAGMA user_version=6; DELETE FROM vec_bounded_memory;")
        .unwrap();
}

#[test]
fn backfill_cost_guard_one_embedder_across_processes() {
    let mock = MockEmbedder::start();
    let tmp = TempDir::new().unwrap();
    let cfg = write_config(tmp.path(), mock.embed_config());
    let db_path = tmp
        .path()
        .join("profiles")
        .join("default")
        .join("memory.db");

    // Seed: library Db creates the schema (user_version=6, jieba+vec registered)
    // and 3 live atoms lacking vectors. memory_write would create 'manual' rows,
    // which are NOT backfill candidates, so seed 'atom' rows directly.
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    {
        let db = Db::open(&db_path).unwrap();
        db.init_schema().unwrap();
        for i in 0..SEEDED_ATOM_COUNT {
            db.conn()
                .execute(
                    "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type) \
                     VALUES ('memory', ?1, 1000, 1000, 'high', 'atom')",
                    [format!("smoke atom {i}")],
                )
                .unwrap();
        }
    }

    // Race gateway + serve on the SAME profile: exactly one backfill-embeds.
    let baseline = mock.hits();
    let _gw = Proc::spawn(&["gateway", "--port", "0"], &cfg, false);
    let _serve = Proc::spawn(&["serve"], &cfg, true);
    wait_for_backfill_convergence(&db_path, "gateway+serve");
    assert_eq!(
        mock.hits() - baseline,
        1,
        "exactly one process must backfill-embed the single batch (3 atoms, batch_size 16)"
    );

    // Kill both predecessors (flock auto-releases on process death), wipe vecs,
    // and a third serve acquires the freed lock and continues to convergence.
    drop(_gw);
    drop(_serve);
    wipe_vec_index(&db_path);
    {
        let _third = Proc::spawn(&["serve"], &cfg, true);
        wait_for_backfill_convergence(&db_path, "third process after kill");
    }
}
