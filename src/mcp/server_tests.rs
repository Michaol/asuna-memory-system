use super::*;

fn test_server() -> (Server, ToolHandler, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        data_dir: tmp.path().to_path_buf(),
        ..Config::default()
    };
    cfg.ensure_dirs().unwrap();
    let db = Rc::new(Db::open_memory().unwrap());
    db.init_schema().unwrap();
    let handler = ToolHandler::new(cfg.clone(), db.clone());
    (Server::new(cfg, db), handler, tmp)
}

/// N6: tools/call 成功路径返回 pretty-printed content 且不带 isError；
/// 工具层错误走 content + isError。序列化失败 fallback 生产不可达
/// （result 已是 Value，无法承载非有限数），仅加固不留静默成功形态。
#[test]
fn test_tools_call_success_and_tool_error_content_shape() {
    let (srv, handler, _tmp) = test_server();
    let resp = srv
        .handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"rebuild_status"}}"#,
            &handler,
        )
        .unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("\"status\": \"idle\""), "pretty text: {text}");
    assert!(resp["result"].get("isError").is_none(), "resp: {resp}");

    let resp = srv
        .handle_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memory_read","arguments":{}}}"#,
            &handler,
        )
        .unwrap();
    assert_eq!(resp["result"]["isError"], json!(true));
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("缺少 target"), "text: {text}");
}

/// MCP 规范（tools / Error Handling）：未知工具名走协议层 -32602，
/// 而非 content+isError（后者仅用于工具执行错误，见上一条测试）。
#[test]
fn test_unknown_tool_returns_protocol_invalid_params() {
    let (srv, handler, _tmp) = test_server();
    let resp = srv
        .handle_line(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}"#,
            &handler,
        )
        .unwrap();
    assert_eq!(resp["error"]["code"], INVALID_PARAMS);
    assert_eq!(resp["id"], 5);
    assert!(resp.get("result").is_none(), "resp: {resp}");
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no_such_tool"),
        "resp: {resp}"
    );
}

#[test]
fn test_read_line_capped_normal_and_oversized_resync() {
    let input = b"{\"a\":1}\n".to_vec();
    let mut cur = io::Cursor::new(input);
    assert_eq!(
        read_line_capped(&mut cur, 1024).unwrap(),
        LineRead::Line("{\"a\":1}\n".to_string())
    );
    assert_eq!(read_line_capped(&mut cur, 1024).unwrap(), LineRead::Eof);

    let oversized_then_valid = b"0123456789abcdef\n{\"ok\":true}\n";
    let mut cur = io::Cursor::new(&oversized_then_valid[..]);
    assert_eq!(read_line_capped(&mut cur, 8).unwrap(), LineRead::TooLarge);
    assert_eq!(
        read_line_capped(&mut cur, 64).unwrap(),
        LineRead::Line("{\"ok\":true}\n".to_string())
    );
    assert_eq!(read_line_capped(&mut cur, 64).unwrap(), LineRead::Eof);
}

#[test]
fn test_jsonrpc_two_phase_parse_and_error_codes() {
    let (srv, handler, _tmp) = test_server();

    // Invalid JSON -> -32700 PARSE_ERROR
    let resp = srv.handle_line("{bad json", &handler).unwrap();
    assert_eq!(resp["error"]["code"], PARSE_ERROR);
    assert!(resp["id"].is_null());

    // Valid JSON, non-object -> -32600 INVALID_REQUEST
    let resp = srv.handle_line("[1, 2, 3]", &handler).unwrap();
    assert_eq!(resp["error"]["code"], INVALID_REQUEST);

    // Wrong jsonrpc version with valid id -> -32600 INVALID_REQUEST keeping id
    let resp = srv
        .handle_line(r#"{"jsonrpc":"1.0","id":7,"method":"ping"}"#, &handler)
        .unwrap();
    assert_eq!(resp["error"]["code"], INVALID_REQUEST);
    assert_eq!(resp["id"], 7);

    // Missing/empty method -> -32600 INVALID_REQUEST
    let resp = srv
        .handle_line(r#"{"jsonrpc":"2.0","id":"r1","method":""}"#, &handler)
        .unwrap();
    assert_eq!(resp["error"]["code"], INVALID_REQUEST);
    assert_eq!(resp["id"], "r1");

    // Illegal id type (object/array) -> -32600 INVALID_REQUEST with id=null
    let resp = srv
        .handle_line(r#"{"jsonrpc":"2.0","id":{},"method":"ping"}"#, &handler)
        .unwrap();
    assert_eq!(resp["error"]["code"], INVALID_REQUEST);
    assert!(resp["id"].is_null());
}

#[test]
fn test_jsonrpc_notifications_ping_and_tools_call_params() {
    let (srv, handler, _tmp) = test_server();

    // Known and unknown notifications (no id) must stay silent
    assert!(srv
        .handle_line(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            &handler
        )
        .is_none());
    assert!(srv
        .handle_line(
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#,
            &handler
        )
        .is_none());

    // ping -> {}
    let ping_resp = srv
        .handle_line(r#"{"jsonrpc":"2.0","id":42,"method":"ping"}"#, &handler)
        .unwrap();
    assert_eq!(ping_resp["id"], 42);
    assert_eq!(ping_resp["result"], json!({}));

    // tools/call with missing/invalid params or non-object arguments -> -32602
    for bad in [
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call"}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":123}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory_read","arguments":"bad"}}"#,
    ] {
        let err = srv.handle_line(bad, &handler).unwrap();
        assert_eq!(err["error"]["code"], INVALID_PARAMS, "input: {bad}");
    }

    // Unknown method -> -32601
    let err = srv
        .handle_line(r#"{"jsonrpc":"2.0","id":9,"method":"no/such"}"#, &handler)
        .unwrap();
    assert_eq!(err["error"]["code"], METHOD_NOT_FOUND);
}

/// M4: "id": null 的 notifications/initialized 保持静默（兼容基线）；带非 null
/// id（1 / "n1"）一律回 -32601 错误响应并保留 id，不得静默丢弃。
#[test]
fn test_initialized_notification_null_id_silent_non_null_id_errors() {
    let (srv, handler, _tmp) = test_server();
    // id:null -> silent (compat with characterization_37 / differential_harness)
    assert!(srv
        .handle_line(
            r#"{"jsonrpc":"2.0","id":null,"method":"notifications/initialized"}"#,
            &handler
        )
        .is_none());
    // non-null id -> METHOD_NOT_FOUND error response, id preserved
    for (line, expected_id) in [
        (
            r#"{"jsonrpc":"2.0","id":1,"method":"notifications/initialized"}"#,
            json!(1),
        ),
        (
            r#"{"jsonrpc":"2.0","id":"n1","method":"notifications/initialized"}"#,
            json!("n1"),
        ),
    ] {
        let resp = srv.handle_line(line, &handler).unwrap();
        assert_eq!(resp["error"]["code"], METHOD_NOT_FOUND, "input: {line}");
        assert_eq!(resp["id"], expected_id, "input: {line}");
        assert!(resp.get("result").is_none(), "input: {line}");
    }
}

/// W25a: 每隔一次 fill_buf 返回 EINTR、且每次最多吐 4 字节的 reader，
/// 覆盖 read_line_capped 与 drain_until_newline 两处重试。
struct InterruptedChunks {
    inner: io::Cursor<Vec<u8>>,
    calls: usize,
}

impl InterruptedChunks {
    fn new(data: &[u8]) -> Self {
        Self {
            inner: io::Cursor::new(data.to_vec()),
            calls: 0,
        }
    }
}

impl io::Read for InterruptedChunks {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl BufRead for InterruptedChunks {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.calls += 1;
        if self.calls % 2 == 1 {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        let buf = self.inner.fill_buf()?;
        Ok(&buf[..buf.len().min(4)])
    }

    fn consume(&mut self, amt: usize) {
        self.inner.consume(amt);
    }
}

#[test]
fn test_read_line_capped_retries_interrupted_fill_buf() {
    let mut reader = InterruptedChunks::new(b"{\"a\":1}\n");
    assert_eq!(
        read_line_capped(&mut reader, 1024).unwrap(),
        LineRead::Line("{\"a\":1}\n".to_string())
    );
    assert_eq!(read_line_capped(&mut reader, 1024).unwrap(), LineRead::Eof);

    // 超长行分片到达：drain_until_newline 内的 EINTR 同样被重试，
    // 且下一个请求保持对齐。
    let mut reader = InterruptedChunks::new(b"0123456789abcdef\n{\"ok\":true}\n");
    assert_eq!(
        read_line_capped(&mut reader, 8).unwrap(),
        LineRead::TooLarge
    );
    assert_eq!(
        read_line_capped(&mut reader, 64).unwrap(),
        LineRead::Line("{\"ok\":true}\n".to_string())
    );
    assert_eq!(read_line_capped(&mut reader, 64).unwrap(), LineRead::Eof);
}

/// W21a 回归：非法 UTF-8 必须被拒（InvalidUtf8 → 协议层 -32700），不得
/// lossy 替换 U+FFFD；后续合法行保持对齐；合法多字节 UTF-8 正常放行。
#[test]
fn test_read_line_capped_rejects_invalid_utf8() {
    let mut cur = io::Cursor::new(b"\xff\xfe\n{\"ok\":true}\n".to_vec());
    assert_eq!(
        read_line_capped(&mut cur, 1024).unwrap(),
        LineRead::InvalidUtf8
    );
    assert_eq!(
        read_line_capped(&mut cur, 1024).unwrap(),
        LineRead::Line("{\"ok\":true}\n".to_string())
    );
    assert_eq!(read_line_capped(&mut cur, 1024).unwrap(), LineRead::Eof);

    // EOF 无换行的非法字节同样拒绝。
    let mut cur = io::Cursor::new(b"\x80abc".to_vec());
    assert_eq!(
        read_line_capped(&mut cur, 1024).unwrap(),
        LineRead::InvalidUtf8
    );

    // 合法多字节 UTF-8 不受影响。
    let mut cur = io::Cursor::new("{\"q\":\"中文\"}\n".as_bytes().to_vec());
    assert_eq!(
        read_line_capped(&mut cur, 1024).unwrap(),
        LineRead::Line("{\"q\":\"中文\"}\n".to_string())
    );
}

/// M4 回归：浮点 id 拒绝（-32600）且不得静默丢弃——即使 method/params 也
/// 非法仍回错误响应；整数（i64/u64 边界）与 string id 不受影响。
#[test]
fn test_jsonrpc_rejects_float_id() {
    let (srv, handler, _tmp) = test_server();
    for line in [
        r#"{"jsonrpc":"2.0","id":1.5,"method":"ping"}"#,
        r#"{"jsonrpc":"2.0","id":1.5,"method":"no/such"}"#,
        r#"{"jsonrpc":"2.0","id":1.5,"method":"tools/call","params":"bad"}"#,
        r#"{"jsonrpc":"2.0","id":-0.5,"method":"ping"}"#,
    ] {
        let resp = srv.handle_line(line, &handler).unwrap();
        assert_eq!(resp["error"]["code"], INVALID_REQUEST, "input: {line}");
        assert!(resp["id"].is_null(), "input: {line}");
    }

    for line in [
        r#"{"jsonrpc":"2.0","id":9223372036854775807,"method":"ping"}"#,
        r#"{"jsonrpc":"2.0","id":18446744073709551615,"method":"ping"}"#,
        r#"{"jsonrpc":"2.0","id":-7,"method":"ping"}"#,
        r#"{"jsonrpc":"2.0","id":"s1","method":"ping"}"#,
    ] {
        let resp = srv.handle_line(line, &handler).unwrap();
        assert_eq!(resp["result"], json!({}), "input: {line}");
    }
}
