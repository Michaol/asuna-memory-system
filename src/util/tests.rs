use super::*;

#[test]
fn test_validate_profile_id() {
    assert!(validate_profile_id("default").is_ok());
    assert!(validate_profile_id("work_2026-main").is_ok());
    assert!(validate_profile_id(&"a".repeat(128)).is_ok());

    assert!(validate_profile_id("").is_err());
    assert!(validate_profile_id(&"a".repeat(129)).is_err());
    assert!(validate_profile_id("../escape").is_err());
    assert!(validate_profile_id("foo/bar").is_err());
    assert!(validate_profile_id("foo\\bar").is_err());
    assert!(validate_profile_id("用户").is_err());
    assert!(validate_profile_id("CON").is_err());
    assert!(validate_profile_id("nul").is_err());
    assert!(validate_profile_id("Com1").is_err());
    assert!(validate_profile_id("lpt9").is_err());
}

#[test]
fn test_validate_session_id() {
    assert!(validate_session_id("sess-123_abc-xyz").is_ok());
    assert!(validate_session_id(&"中".repeat(255)).is_ok());

    assert!(validate_session_id("").is_err());
    assert!(validate_session_id(&"a".repeat(256)).is_err());
    assert!(validate_session_id("a\nb").is_err());
    assert!(validate_session_id("a\0b").is_err());
    assert!(validate_session_id(".").is_err());
    assert!(validate_session_id("..").is_err());
    assert!(validate_session_id("../etc").is_err());
    assert!(validate_session_id("a..b").is_err());
    assert!(validate_session_id("a/b").is_err());
    assert!(validate_session_id("a\\b").is_err());
    // v4-M: ':' 与 profile/task_id 尺标对齐（Windows 文件名非法字符）
    assert!(validate_session_id("sess-123_abc:xyz").is_err());
    assert!(validate_session_id("a:b").is_err());
}

#[test]
fn test_sanitize_url_for_log() {
    assert_eq!(
        sanitize_url_for_log("https://user:secret@host:8080/v1?key=abc#frag"),
        "https://host:8080/v1"
    );
    assert_eq!(
        sanitize_url_for_log("  http://token@localhost:11434/v1#x  "),
        "http://localhost:11434/v1"
    );
    assert_eq!(
        sanitize_url_for_log("https://api.openai.com/v1"),
        "https://api.openai.com/v1"
    );
    assert_eq!(
        sanitize_url_for_log("http://127.0.0.1:8080?secret=1"),
        "http://127.0.0.1:8080"
    );
}

/// F3: scheme-less "user:pass@host/path" must not leak userinfo into logs.
#[test]
fn test_sanitize_url_for_log_scheme_less_userinfo() {
    assert_eq!(
        sanitize_url_for_log("user:pass@host:8080/v1?key=abc#frag"),
        "host:8080/v1"
    );
    assert_eq!(
        sanitize_url_for_log("  token@localhost:11434/v1#x  "),
        "localhost:11434/v1"
    );
    assert_eq!(sanitize_url_for_log("user@host"), "host");
    // no userinfo → unchanged; an '@' in the path is not userinfo
    assert_eq!(
        sanitize_url_for_log("api.example.com/v1"),
        "api.example.com/v1"
    );
    assert_eq!(sanitize_url_for_log("host/path@thing"), "host/path@thing");
}

/// M6 回归：密码含裸 '/' 时不得把整条含凭据 URL 原样落日志。
#[test]
fn test_sanitize_url_userinfo_with_slash_in_password() {
    assert_eq!(
        sanitize_url_for_log("https://user:pa/ss@host:8080/v1"),
        "https://host:8080/v1"
    );
    assert_eq!(
        sanitize_url_for_log("https://user:pa/ss@host"),
        "https://host"
    );
    // path 里的 '@' 仍不是 userinfo（既有契约不变）
    assert_eq!(sanitize_url_for_log("host/path@thing"), "host/path@thing");
}

/// 无 ':' 的 userinfo（base64 basic-auth token 作用户名、无密码）含裸 '/' 时
/// 也必须剥离；同时不得误伤带端口或 path 含 '@' 的正常 URL。
#[test]
fn test_sanitize_url_userinfo_without_colon() {
    assert_eq!(
        sanitize_url_for_log("https://AbCdEf/GhIj@host/v1"),
        "https://host/v1"
    );
    assert_eq!(
        sanitize_url_for_log("https://api.example.com:443/v1/models@latest"),
        "https://api.example.com:443/v1/models@latest"
    );
    assert_eq!(
        sanitize_url_for_log("http://127.0.0.1:11434/v1/user@example.com"),
        "http://127.0.0.1:11434/v1/user@example.com"
    );
    assert_eq!(sanitize_url_for_log("host/path@thing"), "host/path@thing");
}

/// F3-a: 协议相对 "//user:pass@host/path" 与单/三斜杠畸形 URL 同样剥 userinfo。
#[test]
fn test_sanitize_url_for_log_protocol_relative_userinfo() {
    assert_eq!(
        sanitize_url_for_log("//user:pass@host:8080/v1?key=abc#frag"),
        "host:8080/v1"
    );
    assert_eq!(
        sanitize_url_for_log("//token@localhost:11434/v1"),
        "localhost:11434/v1"
    );
    assert_eq!(
        sanitize_url_for_log("https:/user:pass@host/v1?k=1"),
        "https:/host/v1"
    );
    assert_eq!(
        sanitize_url_for_log("https:///user:pass@host:8443/v1"),
        "https:///host:8443/v1"
    );
    assert_eq!(sanitize_url_for_log("///user:pass@host/v1#frag"), "host/v1");
    // 无 userinfo 的协议相对 URL 原样保留
    assert_eq!(sanitize_url_for_log("//host/v1"), "//host/v1");
}

/// NEW-8: recover_poison 恢复后清除 poison 标志，后续普通 lock 不再 poisoned。
#[test]
fn test_recover_poison_clears_poison_flag() {
    let m = std::sync::Mutex::new(1usize);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _g = m.lock().unwrap();
        panic!("poison it");
    }));
    assert!(m.is_poisoned());
    {
        let _g = recover_poison(&m);
    }
    assert!(!m.is_poisoned(), "恢复后 poison 标志应被清除");
    assert!(m.lock().is_ok());
}

#[test]
fn test_truncate_for_log_under_limit_unchanged() {
    assert_eq!(truncate_for_log(""), "");
    assert_eq!(truncate_for_log("short"), "short");
    let exactly_200 = "中".repeat(200);
    assert_eq!(truncate_for_log(&exactly_200), exactly_200);
}

#[test]
fn test_truncate_for_log_cuts_and_marks() {
    let long = "a".repeat(500);
    let out = truncate_for_log(&long);
    assert!(out.starts_with(&"a".repeat(200)));
    assert!(out.contains("(truncated, 500 chars total)"));
    // cut boundary is at 200 chars: the remainder of the input is gone
    assert!(!out.contains(&"a".repeat(201)));
}

#[test]
fn test_truncate_for_log_multibyte_safe() {
    // 201 CJK chars would be byte-cut at an index inside a 3-byte char
    // by a s[..200] implementation; chars().take(200) must not panic.
    let long = "记忆".repeat(120); // 240 chars
    let out = truncate_for_log(&long);
    assert!(out.chars().count() < long.chars().count());
    assert!(out.contains("truncated"));
    assert!(out.starts_with("记忆"));
}

/// C9: userinfo 剥离不得被凭据中的裸 ':'、'/'、'?'、'#' 混淆——authority 边界
/// 判错会把整条含凭据 URL 原样落日志；同时 path 中的 '@'、端口与 IPv6 字面量
/// 不得被误剥。
#[test]
fn test_sanitize_url_for_log_c9_userinfo_edge_cases() {
    // userinfo 含裸 ':'（"user:p:8080"）且 path 中另有 '@'
    assert_eq!(
        sanitize_url_for_log("https://user:p:8080/x@host/v1?tok=s"),
        "https://host/v1"
    );
    // 空密码 + '/' 在密码里
    assert_eq!(
        sanitize_url_for_log("https://user:/pass@host/v1"),
        "https://host/v1"
    );
    // '?' / '#' 出现在 userinfo 密码中：不得把剩余部分当 query/fragment 保留
    assert_eq!(
        sanitize_url_for_log("https://user:p?a=1@host/v1"),
        "https://host/v1"
    );
    assert_eq!(
        sanitize_url_for_log("https://user:p#f@host/v1"),
        "https://host/v1"
    );
    // 无 userinfo 的 path '@' / 端口 / IPv6 字面量不受误伤
    assert_eq!(
        sanitize_url_for_log("https://api.cloudflare.com/models/@cf/meta/llama"),
        "https://api.cloudflare.com/models/@cf/meta/llama"
    );
    assert_eq!(
        sanitize_url_for_log("https://api.example.com:8080/v1@latest"),
        "https://api.example.com:8080/v1@latest"
    );
    assert_eq!(
        sanitize_url_for_log("http://[::1]/v1@tag"),
        "http://[::1]/v1@tag"
    );
}

/// W13: 纯空白 session_id 必须与空串同判 Err。
#[test]
fn test_validate_session_id_whitespace_only_rejected() {
    assert!(validate_session_id("   ").is_err());
    assert!(validate_session_id("\t\n ").is_err());
}

/// W24: 目录本身不算"严格位于自身之内"——containment 语义要求 candidate
/// 是 base 的真子路径（offload 的 task_dir 判定依赖此契约）。
#[test]
fn test_is_contained_in_dir_rejects_equal_dir() {
    let base = tempfile::TempDir::new().unwrap();
    assert!(!is_contained_in_dir(base.path(), base.path()));
}

/// M6a: truncate_for_log 的 ASCII 快路径（字节数 ≤ 上限时原样返回）与
/// "字节数超限但字符数未超限"的多字节安全（不得误截）。
#[test]
fn test_truncate_for_log_ascii_fast_path_and_byte_vs_char_limit() {
    let ascii_200 = "a".repeat(200);
    assert_eq!(truncate_for_log(&ascii_200), ascii_200);
    // 150 个双字节字符：300 字节 > 200，但 150 字符 ≤ 200 → 原样返回
    let multibyte = "é".repeat(150);
    assert_eq!(truncate_for_log(&multibyte), multibyte);
}

#[test]
fn test_url_scheme_issue_matrix() {
    assert!(url_scheme_issue("https://api.openai.com/v1").is_none());
    assert!(url_scheme_issue("HTTPS://example.com").is_none());
    assert!(url_scheme_issue("  https://x  ").is_none());
    assert!(url_scheme_issue("http://localhost:11434/v1").is_some());
    assert!(url_scheme_issue("HTTP://127.0.0.1:9/v1").is_some());
    assert!(url_scheme_issue("ftp://files.example.com").is_some());
    assert!(url_scheme_issue("api.example.com/v1").is_some());
}

#[test]
fn test_atomic_write_creates_and_overwrites_cleanly() {
    let tmp = tempfile::TempDir::new().unwrap();
    let target = tmp.path().join("nested").join("file.md");

    atomic_write(&target, "first payload").unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "first payload");

    atomic_write(&target, "second payload").unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "second payload");

    let leftovers: Vec<_> = std::fs::read_dir(target.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "leftover tmp files: {leftovers:?}");
}

/// NEW-7 残留：启动清扫只删"命中 atomic_write 命名形状 + 超龄"的孤儿，
/// 在途新文件、offload 下划线命名、真实数据与非数字 pid 一律保留。
#[test]
fn test_sweep_orphan_tmp_files() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    let sub = root.join("2026").join("11");
    std::fs::create_dir_all(&sub).unwrap();

    let orphan = root.join(".MEMORY.md.999999.0.tmp");
    let orphan_nested = sub.join(".20261124T100000_abcd1234.jsonl.999999.3.tmp");
    let fresh = root.join(".USER.md.999999.1.tmp");
    let offload = root.join(".offload_999999_1700000000000000000_0.tmp");
    let data = root.join("MEMORY.md");
    let non_numeric = root.join(".MEMORY.md.notpid.0.tmp");
    for p in [
        &orphan,
        &orphan_nested,
        &fresh,
        &offload,
        &data,
        &non_numeric,
    ] {
        std::fs::write(p, "x").unwrap();
    }
    // 除 fresh 外全部做旧（600s > 300s 宽限期）
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
    for p in [&orphan, &orphan_nested, &offload, &data, &non_numeric] {
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }

    let removed = sweep_orphan_tmp_files(root, std::time::Duration::from_secs(300));
    assert_eq!(removed, 2, "只有两个超龄孤儿应被删除");
    assert!(!orphan.exists());
    assert!(!orphan_nested.exists());
    assert!(fresh.exists(), "宽限期内的在途 tmp 不得删除");
    assert!(offload.exists(), "offload 下划线命名不得命中");
    assert!(data.exists(), "真实数据文件不得删除");
    assert!(non_numeric.exists(), "非数字 pid 段不得命中");

    // 不存在的根目录：best-effort 返回 0，不报错
    assert_eq!(
        sweep_orphan_tmp_files(&root.join("missing"), std::time::Duration::from_secs(0)),
        0
    );
}

#[test]
fn test_is_contained_in_dir_guards_traversal() {
    let base = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    let inside_file = base.path().join("a.jsonl");
    let outside_file = outside.path().join("b.jsonl");
    std::fs::write(&inside_file, "{}").unwrap();
    std::fs::write(&outside_file, "{}").unwrap();

    assert!(is_contained_in_dir(base.path(), &inside_file));
    assert!(!is_contained_in_dir(base.path(), &outside_file));
    assert!(!is_contained_in_dir(
        base.path(),
        &base.path().join("nonexistent.jsonl")
    ));
}

#[test]
fn test_recover_poison_recovers_data() {
    let mutex = std::sync::Mutex::new(42usize);
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut g = mutex.lock().unwrap();
        *g = 99;
        panic!("intentional poison");
    }));
    assert!(res.is_err());
    let recovered = recover_poison(&mutex);
    assert_eq!(*recovered, 99);
}

#[test]
fn test_valid_roles_contains_expected_set() {
    assert_eq!(VALID_ROLES, &["user", "assistant", "tool_call", "system"]);
}
