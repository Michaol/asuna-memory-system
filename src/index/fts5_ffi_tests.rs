use super::{
    register_tokenizer, register_tokenizer_on_api, x_create, x_delete, x_destroy, x_tokenize,
    RegisterTokenizerError, TokenizeReason, Tokenizer,
};
use rusqlite::ffi::{
    fts5_api, fts5_tokenizer_v2, Fts5Tokenizer, FTS5_TOKENIZE_DOCUMENT, FTS5_TOKENIZE_PREFIX,
    FTS5_TOKENIZE_QUERY, SQLITE_CONSTRAINT, SQLITE_ERROR, SQLITE_OK,
};
use rusqlite::Connection;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::ops::Range;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct CaptureGlobal {
    seen_texts: Mutex<Vec<(TokenizeReason, Vec<u8>)>>,
    seen_args: Mutex<Vec<Vec<String>>>,
}

struct CaptureTokenizer {
    global: Arc<CaptureGlobal>,
}

impl Tokenizer for CaptureTokenizer {
    type Global = Arc<CaptureGlobal>;

    fn name() -> &'static CStr {
        c"capture_tok"
    }

    fn new(global: &Self::Global, args: Vec<String>) -> Result<Self, rusqlite::Error> {
        global.seen_args.lock().unwrap().push(args);
        Ok(Self {
            global: Arc::clone(global),
        })
    }

    fn tokenize<TKF>(
        &mut self,
        reason: TokenizeReason,
        text: &[u8],
        mut push_token: TKF,
    ) -> Result<(), rusqlite::Error>
    where
        TKF: FnMut(&[u8], Range<usize>, bool) -> Result<(), rusqlite::Error>,
    {
        self.global
            .seen_texts
            .lock()
            .unwrap()
            .push((reason, text.to_vec()));
        if !text.is_empty() {
            push_token(text, 0..text.len(), false)?;
        }
        Ok(())
    }
}

struct BadRangeTokenizer;

impl Tokenizer for BadRangeTokenizer {
    type Global = ();

    fn name() -> &'static CStr {
        c"bad_range_tok"
    }

    fn new(_global: &Self::Global, _args: Vec<String>) -> Result<Self, rusqlite::Error> {
        Ok(Self)
    }

    fn tokenize<TKF>(
        &mut self,
        _reason: TokenizeReason,
        _text: &[u8],
        mut push_token: TKF,
    ) -> Result<(), rusqlite::Error>
    where
        TKF: FnMut(&[u8], Range<usize>, bool) -> Result<(), rusqlite::Error>,
    {
        push_token(b"tok", 0..99, false)
    }
}

struct PanickingCreateTokenizer;

impl Tokenizer for PanickingCreateTokenizer {
    type Global = ();

    fn name() -> &'static CStr {
        c"panic_create_tok"
    }

    fn new(_global: &Self::Global, _args: Vec<String>) -> Result<Self, rusqlite::Error> {
        panic!("boom inside new");
    }

    fn tokenize<TKF>(
        &mut self,
        _reason: TokenizeReason,
        _text: &[u8],
        _push_token: TKF,
    ) -> Result<(), rusqlite::Error>
    where
        TKF: FnMut(&[u8], Range<usize>, bool) -> Result<(), rusqlite::Error>,
    {
        Ok(())
    }
}

struct PanickingTokenizeTokenizer;

impl Tokenizer for PanickingTokenizeTokenizer {
    type Global = ();

    fn name() -> &'static CStr {
        c"panic_tok"
    }

    fn new(_global: &Self::Global, _args: Vec<String>) -> Result<Self, rusqlite::Error> {
        Ok(Self)
    }

    fn tokenize<TKF>(
        &mut self,
        _reason: TokenizeReason,
        _text: &[u8],
        _push_token: TKF,
    ) -> Result<(), rusqlite::Error>
    where
        TKF: FnMut(&[u8], Range<usize>, bool) -> Result<(), rusqlite::Error>,
    {
        panic!("boom inside tokenize");
    }
}

struct DropGlobal {
    global_drops: Arc<AtomicUsize>,
    instance_drops: Arc<AtomicUsize>,
}

impl Drop for DropGlobal {
    fn drop(&mut self) {
        self.global_drops.fetch_add(1, Ordering::SeqCst);
    }
}

struct DropTrackingTokenizer {
    instance_drops: Arc<AtomicUsize>,
}

impl Drop for DropTrackingTokenizer {
    fn drop(&mut self) {
        self.instance_drops.fetch_add(1, Ordering::SeqCst);
    }
}

impl Tokenizer for DropTrackingTokenizer {
    type Global = DropGlobal;

    fn name() -> &'static CStr {
        c"drop_track"
    }

    fn new(global: &Self::Global, _args: Vec<String>) -> Result<Self, rusqlite::Error> {
        Ok(Self {
            instance_drops: Arc::clone(&global.instance_drops),
        })
    }

    fn tokenize<TKF>(
        &mut self,
        _reason: TokenizeReason,
        text: &[u8],
        mut push_token: TKF,
    ) -> Result<(), rusqlite::Error>
    where
        TKF: FnMut(&[u8], Range<usize>, bool) -> Result<(), rusqlite::Error>,
    {
        let Ok(s) = std::str::from_utf8(text) else {
            return Ok(());
        };
        let mut offset = 0usize;
        for part in s.split_inclusive(char::is_whitespace) {
            let trimmed = part.trim_end_matches(char::is_whitespace);
            if !trimmed.is_empty() {
                push_token(trimmed.as_bytes(), offset..offset + trimmed.len(), false)?;
            }
            offset += part.len();
        }
        Ok(())
    }
}

unsafe extern "C" fn record_pushed_token(
    ctx: *mut c_void,
    _tflags: c_int,
    token: *const c_char,
    n_token: c_int,
    i_start: c_int,
    i_end: c_int,
) -> c_int {
    if !ctx.is_null() && !token.is_null() && n_token >= 0 {
        let out = unsafe { &mut *ctx.cast::<Vec<(Vec<u8>, c_int, c_int)>>() };
        let bytes = unsafe { std::slice::from_raw_parts(token.cast::<u8>(), n_token as usize) };
        out.push((bytes.to_vec(), i_start, i_end));
    }
    SQLITE_OK
}

unsafe extern "C" fn mock_fail_create_tokenizer_v2(
    _api: *mut fts5_api,
    _name: *const c_char,
    user_data: *mut c_void,
    _tokenizer: *mut fts5_tokenizer_v2,
    x_destroy_fn: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    if let Some(destroy) = x_destroy_fn {
        unsafe { destroy(user_data) };
    }
    SQLITE_ERROR
}

#[test]
fn test_x_tokenize_handles_null_and_non_positive_data_without_ub() {
    let global = Arc::new(CaptureGlobal {
        seen_texts: Mutex::new(Vec::new()),
        seen_args: Mutex::new(Vec::new()),
    });
    let global_ptr = Box::into_raw(Box::new(Arc::clone(&global))).cast::<c_void>();

    // Null global / out_tokenizer guards + null-safe x_delete/x_destroy no-ops.
    let mut dummy_tok: *mut Fts5Tokenizer = ptr::null_mut();
    assert_eq!(
        unsafe {
            x_create::<CaptureTokenizer>(ptr::null_mut(), ptr::null_mut(), 0, &raw mut dummy_tok)
        },
        SQLITE_ERROR
    );
    assert_eq!(
        unsafe { x_create::<CaptureTokenizer>(global_ptr, ptr::null_mut(), 0, ptr::null_mut()) },
        SQLITE_ERROR
    );
    unsafe {
        x_delete::<CaptureTokenizer>(ptr::null_mut());
        x_destroy::<CaptureTokenizer>(ptr::null_mut());
    }

    // x_create with null args / negative nargs / mixed null element.
    let mut tok_null_args: *mut Fts5Tokenizer = ptr::null_mut();
    assert_eq!(
        unsafe {
            x_create::<CaptureTokenizer>(global_ptr, ptr::null_mut(), -1, &raw mut tok_null_args)
        },
        SQLITE_OK
    );
    let arg_opt = c"disable_stopword";
    let mut raw_args = [ptr::null(), arg_opt.as_ptr()];
    let mut tok_with_args: *mut Fts5Tokenizer = ptr::null_mut();
    assert_eq!(
        unsafe {
            x_create::<CaptureTokenizer>(
                global_ptr,
                raw_args.as_mut_ptr(),
                2,
                &raw mut tok_with_args,
            )
        },
        SQLITE_OK
    );

    let mut emitted: Vec<(Vec<u8>, c_int, c_int)> = Vec::new();
    let ctx = (&raw mut emitted).cast::<c_void>();
    let non_null = b"abc".as_ptr().cast::<c_char>();
    for (ptr_val, len_val) in [
        (ptr::null(), 0),
        (ptr::null(), -5),
        (non_null, 0),
        (non_null, -1),
    ] {
        let rc = unsafe {
            x_tokenize::<CaptureTokenizer>(
                tok_null_args,
                ctx,
                FTS5_TOKENIZE_DOCUMENT,
                ptr_val,
                len_val,
                ptr::null(),
                0,
                Some(record_pushed_token),
            )
        };
        assert_eq!(rc, SQLITE_OK);
    }

    let rc = unsafe {
        x_tokenize::<CaptureTokenizer>(
            tok_null_args,
            ctx,
            FTS5_TOKENIZE_QUERY | FTS5_TOKENIZE_PREFIX,
            b"rust".as_ptr().cast::<c_char>(),
            4,
            ptr::null(),
            0,
            Some(record_pushed_token),
        )
    };
    assert_eq!(rc, SQLITE_OK);
    assert_eq!(emitted, vec![(b"rust".to_vec(), 0, 4)]);

    let seen = global.seen_texts.lock().unwrap();
    assert_eq!(seen.len(), 5);
    for entry in &seen[..4] {
        assert_eq!(entry.0, TokenizeReason::Document);
        assert!(entry.1.is_empty());
    }
    assert_eq!(
        seen[4],
        (TokenizeReason::Query { prefix: true }, b"rust".to_vec())
    );
    drop(seen);

    let args_seen = global.seen_args.lock().unwrap();
    assert_eq!(
        *args_seen,
        vec![Vec::<String>::new(), vec!["disable_stopword".to_string()]]
    );
    drop(args_seen);

    unsafe {
        x_delete::<CaptureTokenizer>(tok_null_args);
        x_delete::<CaptureTokenizer>(tok_with_args);
        x_destroy::<CaptureTokenizer>(global_ptr);
    }
}

#[test]
fn test_x_tokenize_rejects_invalid_flag_and_null_callback() {
    let global_ptr = Box::into_raw(Box::new(Arc::new(CaptureGlobal {
        seen_texts: Mutex::new(Vec::new()),
        seen_args: Mutex::new(Vec::new()),
    })))
    .cast::<c_void>();
    let mut tok: *mut Fts5Tokenizer = ptr::null_mut();
    assert_eq!(
        unsafe { x_create::<CaptureTokenizer>(global_ptr, ptr::null_mut(), 0, &raw mut tok) },
        SQLITE_OK
    );

    assert_eq!(
        unsafe {
            x_tokenize::<CaptureTokenizer>(
                ptr::null_mut(),
                ptr::null_mut(),
                FTS5_TOKENIZE_DOCUMENT,
                ptr::null(),
                0,
                ptr::null(),
                0,
                Some(record_pushed_token),
            )
        },
        SQLITE_ERROR
    );
    assert_eq!(
        unsafe {
            x_tokenize::<CaptureTokenizer>(
                tok,
                ptr::null_mut(),
                FTS5_TOKENIZE_DOCUMENT,
                ptr::null(),
                0,
                ptr::null(),
                0,
                None,
            )
        },
        SQLITE_ERROR
    );
    assert_eq!(
        unsafe {
            x_tokenize::<CaptureTokenizer>(
                tok,
                ptr::null_mut(),
                0x7fff,
                ptr::null(),
                0,
                ptr::null(),
                0,
                Some(record_pushed_token),
            )
        },
        SQLITE_ERROR
    );

    // Out-of-bounds token range is converted into SQLITE_ERROR rather than panicking.
    let unit_global = Box::into_raw(Box::new(())).cast::<c_void>();
    let mut bad_tok: *mut Fts5Tokenizer = ptr::null_mut();
    assert_eq!(
        unsafe { x_create::<BadRangeTokenizer>(unit_global, ptr::null_mut(), 0, &raw mut bad_tok) },
        SQLITE_OK
    );
    assert_eq!(
        unsafe {
            x_tokenize::<BadRangeTokenizer>(
                bad_tok,
                ptr::null_mut(),
                FTS5_TOKENIZE_DOCUMENT,
                b"hi".as_ptr().cast::<c_char>(),
                2,
                ptr::null(),
                0,
                Some(record_pushed_token),
            )
        },
        SQLITE_ERROR
    );

    unsafe {
        x_delete::<BadRangeTokenizer>(bad_tok);
        x_destroy::<BadRangeTokenizer>(unit_global);
        x_delete::<CaptureTokenizer>(tok);
        x_destroy::<CaptureTokenizer>(global_ptr);
    }
}

#[test]
fn test_x_tokenize_and_x_create_catch_panic_and_return_sqlite_error() {
    let unit_global = Box::into_raw(Box::new(())).cast::<c_void>();
    let mut out_panic_create: *mut Fts5Tokenizer = ptr::null_mut();
    assert_eq!(
        unsafe {
            x_create::<PanickingCreateTokenizer>(
                unit_global,
                ptr::null_mut(),
                0,
                &raw mut out_panic_create,
            )
        },
        SQLITE_ERROR
    );
    assert!(out_panic_create.is_null());

    let mut out_tok: *mut Fts5Tokenizer = ptr::null_mut();
    assert_eq!(
        unsafe {
            x_create::<PanickingTokenizeTokenizer>(
                unit_global,
                ptr::null_mut(),
                0,
                &raw mut out_tok,
            )
        },
        SQLITE_OK
    );
    assert_eq!(
        unsafe {
            x_tokenize::<PanickingTokenizeTokenizer>(
                out_tok,
                ptr::null_mut(),
                FTS5_TOKENIZE_DOCUMENT,
                b"trigger panic".as_ptr().cast::<c_char>(),
                13,
                ptr::null(),
                0,
                Some(record_pushed_token),
            )
        },
        SQLITE_ERROR
    );

    unsafe {
        x_delete::<PanickingTokenizeTokenizer>(out_tok);
        x_destroy::<PanickingTokenizeTokenizer>(unit_global);
    }
}

#[test]
fn test_lifecycle_x_create_x_delete_x_destroy_drop_symmetry() {
    let global_drops = Arc::new(AtomicUsize::new(0));
    let instance_drops = Arc::new(AtomicUsize::new(0));

    let conn = Connection::open_in_memory().unwrap();
    register_tokenizer::<DropTrackingTokenizer>(
        &conn,
        DropGlobal {
            global_drops: Arc::clone(&global_drops),
            instance_drops: Arc::clone(&instance_drops),
        },
    )
    .unwrap();

    conn.execute(
        "CREATE VIRTUAL TABLE t_drop USING fts5(content, tokenize='drop_track')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO t_drop(content) VALUES (''), ('hello fts5 world')",
        [],
    )
    .unwrap();

    let hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM t_drop WHERE t_drop MATCH 'hello'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1);

    let prefix_hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM t_drop WHERE t_drop MATCH 'wor*'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(prefix_hits, 1);

    conn.execute("DROP TABLE t_drop", []).unwrap();
    conn.close().unwrap();

    assert!(
        instance_drops.load(Ordering::SeqCst) >= 1,
        "tokenizer instances must be dropped symmetrically via xDelete"
    );
    assert_eq!(
        global_drops.load(Ordering::SeqCst),
        1,
        "global user_data must be dropped once via xDestroy on connection close"
    );
}

#[test]
fn test_register_tokenizer_on_api_errors_do_not_leak_global() {
    let make_global = || {
        let g = Arc::new(AtomicUsize::new(0));
        let i = Arc::new(AtomicUsize::new(0));
        let dg = DropGlobal {
            global_drops: Arc::clone(&g),
            instance_drops: Arc::clone(&i),
        };
        (dg, g)
    };

    // 1. iVersion < 3 must fail BEFORE Box::into_raw, dropping global immediately.
    let (dg1, g1) = make_global();
    let mut low_api = fts5_api {
        iVersion: 2,
        xCreateTokenizer: None,
        xFindTokenizer: None,
        xCreateFunction: None,
        xCreateTokenizer_v2: None,
        xFindTokenizer_v2: None,
    };
    let err1 = unsafe { register_tokenizer_on_api::<DropTrackingTokenizer>(&raw mut low_api, dg1) }
        .unwrap_err();
    assert_eq!(err1, RegisterTokenizerError::Fts5ApiVersionTooLow);
    assert_eq!(g1.load(Ordering::SeqCst), 1);

    // 2. Null xCreateTokenizer_v2 must fail BEFORE Box::into_raw, dropping global immediately.
    let (dg2, g2) = make_global();
    let mut null_fn_api = fts5_api {
        iVersion: 3,
        xCreateTokenizer: None,
        xFindTokenizer: None,
        xCreateFunction: None,
        xCreateTokenizer_v2: None,
        xFindTokenizer_v2: None,
    };
    let err2 =
        unsafe { register_tokenizer_on_api::<DropTrackingTokenizer>(&raw mut null_fn_api, dg2) }
            .unwrap_err();
    assert_eq!(err2, RegisterTokenizerError::Fts5xCreateTokenizerV2Nul);
    assert_eq!(g2.load(Ordering::SeqCst), 1);

    // 3. xCreateTokenizer_v2 error: SQLite xDestroy releases global once (no double-free / leak).
    let (dg3, g3) = make_global();
    let mut err_api = fts5_api {
        iVersion: 3,
        xCreateTokenizer: None,
        xFindTokenizer: None,
        xCreateFunction: None,
        xCreateTokenizer_v2: Some(mock_fail_create_tokenizer_v2),
        xFindTokenizer_v2: None,
    };
    let err3 = unsafe { register_tokenizer_on_api::<DropTrackingTokenizer>(&raw mut err_api, dg3) }
        .unwrap_err();
    assert_eq!(
        err3,
        RegisterTokenizerError::Fts5xCreateTokenizerFailed(SQLITE_ERROR)
    );
    assert_eq!(g3.load(Ordering::SeqCst), 1);
}

/// W7: `new` 返回 extended_code == 0（SQLITE_OK）的 SqliteFailure。
struct ZeroCodeCreateTokenizer;

impl Tokenizer for ZeroCodeCreateTokenizer {
    type Global = ();

    fn name() -> &'static CStr {
        c"zero_code_create_tok"
    }

    fn new(_global: &Self::Global, _args: Vec<String>) -> Result<Self, rusqlite::Error> {
        Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(SQLITE_OK),
            None,
        ))
    }

    fn tokenize<TKF>(
        &mut self,
        _reason: TokenizeReason,
        _text: &[u8],
        _push_token: TKF,
    ) -> Result<(), rusqlite::Error>
    where
        TKF: FnMut(&[u8], Range<usize>, bool) -> Result<(), rusqlite::Error>,
    {
        Ok(())
    }
}

/// W7: `tokenize` 返回 extended_code == 0（SQLITE_OK）的 SqliteFailure。
struct ZeroCodeTokenizeTokenizer;

impl Tokenizer for ZeroCodeTokenizeTokenizer {
    type Global = ();

    fn name() -> &'static CStr {
        c"zero_code_tok"
    }

    fn new(_global: &Self::Global, _args: Vec<String>) -> Result<Self, rusqlite::Error> {
        Ok(Self)
    }

    fn tokenize<TKF>(
        &mut self,
        _reason: TokenizeReason,
        _text: &[u8],
        _push_token: TKF,
    ) -> Result<(), rusqlite::Error>
    where
        TKF: FnMut(&[u8], Range<usize>, bool) -> Result<(), rusqlite::Error>,
    {
        Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(SQLITE_OK),
            None,
        ))
    }
}

/// W7: `new` 返回非零 extended_code（SQLITE_CONSTRAINT）的 SqliteFailure。
struct ConstraintCreateTokenizer;

impl Tokenizer for ConstraintCreateTokenizer {
    type Global = ();

    fn name() -> &'static CStr {
        c"constraint_create_tok"
    }

    fn new(_global: &Self::Global, _args: Vec<String>) -> Result<Self, rusqlite::Error> {
        Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(SQLITE_CONSTRAINT),
            None,
        ))
    }

    fn tokenize<TKF>(
        &mut self,
        _reason: TokenizeReason,
        _text: &[u8],
        _push_token: TKF,
    ) -> Result<(), rusqlite::Error>
    where
        TKF: FnMut(&[u8], Range<usize>, bool) -> Result<(), rusqlite::Error>,
    {
        Ok(())
    }
}

/// W7: `tokenize` 返回非零 extended_code（SQLITE_CONSTRAINT）的 SqliteFailure。
struct ConstraintTokenizeTokenizer;

impl Tokenizer for ConstraintTokenizeTokenizer {
    type Global = ();

    fn name() -> &'static CStr {
        c"constraint_tok"
    }

    fn new(_global: &Self::Global, _args: Vec<String>) -> Result<Self, rusqlite::Error> {
        Ok(Self)
    }

    fn tokenize<TKF>(
        &mut self,
        _reason: TokenizeReason,
        _text: &[u8],
        _push_token: TKF,
    ) -> Result<(), rusqlite::Error>
    where
        TKF: FnMut(&[u8], Range<usize>, bool) -> Result<(), rusqlite::Error>,
    {
        Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(SQLITE_CONSTRAINT),
            None,
        ))
    }
}

/// W7 回归：extended_code == 0 不得伪装成功——x_create/x_tokenize 均须映射
/// 为 SQLITE_ERROR；非零 extended_code 原样透传。
#[test]
fn test_zero_extended_code_maps_to_sqlite_error_not_ok() {
    let unit_global = Box::into_raw(Box::new(())).cast::<c_void>();

    // x_create: new 返回 ext=0 → SQLITE_ERROR，且不得产出 tokenizer 实例
    let mut out_create: *mut Fts5Tokenizer = ptr::null_mut();
    assert_eq!(
        unsafe {
            x_create::<ZeroCodeCreateTokenizer>(
                unit_global,
                ptr::null_mut(),
                0,
                &raw mut out_create,
            )
        },
        SQLITE_ERROR
    );
    assert!(out_create.is_null());

    // x_tokenize: tokenize 返回 ext=0 → SQLITE_ERROR
    let mut tok: *mut Fts5Tokenizer = ptr::null_mut();
    assert_eq!(
        unsafe {
            x_create::<ZeroCodeTokenizeTokenizer>(unit_global, ptr::null_mut(), 0, &raw mut tok)
        },
        SQLITE_OK
    );
    assert_eq!(
        unsafe {
            x_tokenize::<ZeroCodeTokenizeTokenizer>(
                tok,
                ptr::null_mut(),
                FTS5_TOKENIZE_DOCUMENT,
                b"hi".as_ptr().cast::<c_char>(),
                2,
                ptr::null(),
                0,
                Some(record_pushed_token),
            )
        },
        SQLITE_ERROR
    );

    unsafe {
        x_delete::<ZeroCodeTokenizeTokenizer>(tok);
        x_destroy::<ZeroCodeTokenizeTokenizer>(unit_global);
    }
}

/// W7 回归：非零 extended_code（SQLITE_CONSTRAINT）原样透传，不被吞成 SQLITE_ERROR。
#[test]
fn test_nonzero_extended_code_passes_through() {
    let unit_global = Box::into_raw(Box::new(())).cast::<c_void>();

    let mut out: *mut Fts5Tokenizer = ptr::null_mut();
    assert_eq!(
        unsafe {
            x_create::<ConstraintCreateTokenizer>(unit_global, ptr::null_mut(), 0, &raw mut out)
        },
        SQLITE_CONSTRAINT
    );
    assert!(out.is_null());

    let mut tok: *mut Fts5Tokenizer = ptr::null_mut();
    assert_eq!(
        unsafe {
            x_create::<ConstraintTokenizeTokenizer>(unit_global, ptr::null_mut(), 0, &raw mut tok)
        },
        SQLITE_OK
    );
    assert_eq!(
        unsafe {
            x_tokenize::<ConstraintTokenizeTokenizer>(
                tok,
                ptr::null_mut(),
                FTS5_TOKENIZE_DOCUMENT,
                b"hi".as_ptr().cast::<c_char>(),
                2,
                ptr::null(),
                0,
                Some(record_pushed_token),
            )
        },
        SQLITE_CONSTRAINT
    );

    unsafe {
        x_delete::<ConstraintTokenizeTokenizer>(tok);
        x_destroy::<ConstraintTokenizeTokenizer>(unit_global);
    }
}
