// Hardened in-process FTS5 v2 tokenizer FFI shim, adapted from upstream
// `sqlite-simple-tokenizer` / ColonelThirtyTwo's FTS5 v2 wrapper gist
// (https://gist.github.com/ColonelThirtyTwo/3dd1fe04e4cff0502fa70d12f3a6e72e,
// dual-licensed MIT OR Apache-2.0; see NOTICE.md and LICENSES/sqlite-simple-tokenizer-MIT).

use rusqlite::ffi::{
    fts5_api, fts5_tokenizer_v2, sqlite3_bind_pointer, sqlite3_finalize, sqlite3_prepare_v3,
    sqlite3_step, sqlite3_stmt, Fts5Tokenizer, FTS5_TOKENIZE_AUX, FTS5_TOKENIZE_DOCUMENT,
    FTS5_TOKENIZE_PREFIX, FTS5_TOKENIZE_QUERY, FTS5_TOKEN_COLOCATED, SQLITE_DONE, SQLITE_ERROR,
    SQLITE_OK, SQLITE_ROW,
};
use rusqlite::Connection;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::fmt::Formatter;
use std::ops::Range;
use std::panic::AssertUnwindSafe;

/// Minimum `fts5_api.iVersion` required for `xCreateTokenizer_v2`.
const FTS5_API_VERSION: c_int = 3;
/// `fts5_tokenizer_v2.iVersion` interface version expected by SQLite FTS5.
const FTS5_TOKENIZER_VERSION: c_int = 2;

/// Reason FTS5 invoked the tokenizer callback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TokenizeReason {
    /// Inserting or deleting document content in an FTS5 table.
    Document,
    /// Evaluating a `MATCH` query (with optional `*` prefix suffix).
    Query {
        #[allow(dead_code)]
        prefix: bool,
    },
    /// Invoked via auxiliary `fts5_api.xTokenize`.
    Aux,
}

/// Error returned when SQLite passes an unrecognized `flags` bitmask to `xTokenize`.
#[derive(Debug)]
pub enum IntoTokenizeReasonError {
    UnrecognizedValue(c_int),
}

impl std::fmt::Display for IntoTokenizeReasonError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnrecognizedValue(flag) => {
                write!(f, "Unrecognized flags passed to xTokenize: {flag}")
            }
        }
    }
}

impl std::error::Error for IntoTokenizeReasonError {}

impl TryFrom<c_int> for TokenizeReason {
    type Error = IntoTokenizeReasonError;

    fn try_from(value: c_int) -> Result<Self, Self::Error> {
        const FTS5_TOKENIZE_QUERY_PREFIX: c_int = FTS5_TOKENIZE_QUERY | FTS5_TOKENIZE_PREFIX;
        match value {
            FTS5_TOKENIZE_DOCUMENT => Ok(Self::Document),
            FTS5_TOKENIZE_QUERY => Ok(Self::Query { prefix: false }),
            FTS5_TOKENIZE_QUERY_PREFIX => Ok(Self::Query { prefix: true }),
            FTS5_TOKENIZE_AUX => Ok(Self::Aux),
            _ => Err(IntoTokenizeReasonError::UnrecognizedValue(value)),
        }
    }
}

/// Safe Rust trait wrapped by the SQLite FTS5 v2 tokenizer FFI trampolines.
pub trait Tokenizer: Sized + Send + 'static {
    type Global: Send + 'static;

    fn name() -> &'static CStr;

    fn new(global: &Self::Global, args: Vec<String>) -> Result<Self, rusqlite::Error>;

    fn tokenize<TKF>(
        &mut self,
        reason: TokenizeReason,
        text: &[u8],
        push_token: TKF,
    ) -> Result<(), rusqlite::Error>
    where
        TKF: FnMut(&[u8], Range<usize>, bool) -> Result<(), rusqlite::Error>;
}

fn panic_err_to_str(msg: &(dyn std::any::Any + Send)) -> &str {
    if let Some(msg) = msg.downcast_ref::<String>() {
        msg.as_str()
    } else if let Some(msg) = msg.downcast_ref::<&'static str>() {
        msg
    } else {
        "<non-string panic reason>"
    }
}

unsafe extern "C" fn x_create<T: Tokenizer>(
    global: *mut c_void,
    args: *mut *const c_char,
    nargs: c_int,
    out_tokenizer: *mut *mut Fts5Tokenizer,
) -> c_int {
    if global.is_null() || out_tokenizer.is_null() {
        return SQLITE_ERROR;
    }
    let global = unsafe { &*global.cast::<T::Global>() };
    // M8a: args 的 CStr 提取也放进 catch_unwind——参数解析期间的任何 panic
    // 都不得穿越 extern "C" 边界 unwind。
    let res = std::panic::catch_unwind(AssertUnwindSafe(move || {
        let args: Vec<String> = if args.is_null() || nargs <= 0 {
            Vec::new()
        } else {
            (0..nargs as usize)
                .map(|i| unsafe { *args.add(i) })
                .filter(|p| !p.is_null())
                .map(|s| unsafe { CStr::from_ptr(s).to_string_lossy().into_owned() })
                .collect()
        };
        T::new(global, args)
    }));
    match res {
        Ok(Ok(v)) => {
            let bp = Box::into_raw(Box::new(v));
            unsafe {
                *out_tokenizer = bp.cast::<Fts5Tokenizer>();
            }
            SQLITE_OK
        }
        // W7: extended_code 为 0（SQLITE_OK）时不得伪装成功——映射为 SQLITE_ERROR。
        Ok(Err(rusqlite::Error::SqliteFailure(e, _))) => {
            if e.extended_code == SQLITE_OK {
                SQLITE_ERROR
            } else {
                e.extended_code
            }
        }
        Ok(Err(_)) => SQLITE_ERROR,
        Err(msg) => {
            tracing::error!(
                "<{} as Tokenizer>::new panic: {}",
                std::any::type_name::<T>(),
                panic_err_to_str(msg.as_ref())
            );
            SQLITE_ERROR
        }
    }
}

unsafe extern "C" fn x_delete<T: Tokenizer>(v: *mut Fts5Tokenizer) {
    if v.is_null() {
        return;
    }
    let tokenizer = unsafe { Box::from_raw(v.cast::<T>()) };
    if let Err(e) = std::panic::catch_unwind(AssertUnwindSafe(move || drop(tokenizer))) {
        tracing::error!(
            "{}::drop panic: {}",
            std::any::type_name::<T>(),
            panic_err_to_str(e.as_ref())
        );
    }
}

unsafe extern "C" fn x_destroy<T: Tokenizer>(v: *mut c_void) {
    if v.is_null() {
        return;
    }
    let global = unsafe { Box::from_raw(v.cast::<T::Global>()) };
    if let Err(e) = std::panic::catch_unwind(AssertUnwindSafe(move || drop(global))) {
        tracing::error!(
            "{}::drop panic: {}",
            std::any::type_name::<T::Global>(),
            panic_err_to_str(e.as_ref())
        );
    }
}

unsafe extern "C" fn x_tokenize<T: Tokenizer>(
    this: *mut Fts5Tokenizer,
    ctx: *mut c_void,
    flag: c_int,
    data: *const c_char,
    data_len: c_int,
    _locale: *const c_char,
    _locale_len: c_int,
    push_token: Option<
        unsafe extern "C" fn(*mut c_void, c_int, *const c_char, c_int, c_int, c_int) -> c_int,
    >,
) -> c_int {
    if this.is_null() {
        tracing::error!("x_tokenize received null tokenizer pointer");
        return SQLITE_ERROR;
    }
    let Some(push_cb) = push_token else {
        tracing::error!("x_tokenize received null push_token callback");
        return SQLITE_ERROR;
    };
    let this = unsafe { &mut *this.cast::<T>() };
    let reason = match TokenizeReason::try_from(flag) {
        Ok(reason) => reason,
        Err(error) => {
            tracing::error!("{error}");
            return SQLITE_ERROR;
        }
    };

    // Guard against NULL / non-positive pointer+len from SQLite on empty inputs
    // (`slice::from_raw_parts(null, 0)` triggers UB / debug panic since Rust 1.78).
    let data: &[u8] = if data.is_null() || data_len <= 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(data.cast::<u8>(), data_len as usize) }
    };

    let push_token_fn = |token: &[u8],
                         Range { start, end }: Range<usize>,
                         colocated: bool|
     -> Result<(), rusqlite::Error> {
        if start > end || end > data.len() {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(SQLITE_ERROR),
                Some(format!(
                    "Token range [{start}..{end}] is invalid for data length {}",
                    data.len()
                )),
            ));
        }
        let Ok(token_len) = c_int::try_from(token.len()) else {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(SQLITE_ERROR),
                Some("Token exceeds c_int::MAX".to_string()),
            ));
        };
        let (Ok(start_c), Ok(end_c)) = (c_int::try_from(start), c_int::try_from(end)) else {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(SQLITE_ERROR),
                Some("Token offset exceeds c_int::MAX".to_string()),
            ));
        };
        let flags = if colocated { FTS5_TOKEN_COLOCATED } else { 0 };

        let res = unsafe {
            push_cb(
                ctx,
                flags,
                token.as_ptr().cast::<c_char>(),
                token_len,
                start_c,
                end_c,
            )
        };
        if res == SQLITE_OK {
            Ok(())
        } else {
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(res),
                None,
            ))
        }
    };

    match std::panic::catch_unwind(AssertUnwindSafe(|| {
        this.tokenize(reason, data, push_token_fn)
    })) {
        Ok(Ok(())) => SQLITE_OK,
        // W7: extended_code 为 0（SQLITE_OK）时不得伪装成功——映射为 SQLITE_ERROR。
        Ok(Err(rusqlite::Error::SqliteFailure(e, _))) => {
            if e.extended_code == SQLITE_OK {
                SQLITE_ERROR
            } else {
                e.extended_code
            }
        }
        Ok(Err(_)) => SQLITE_ERROR,
        Err(msg) => {
            tracing::error!(
                "<{} as Tokenizer>::tokenize panic: {}",
                std::any::type_name::<T>(),
                panic_err_to_str(msg.as_ref())
            );
            SQLITE_ERROR
        }
    }
}

/// Errors produced when querying or registering an FTS5 v2 tokenizer.
#[derive(Debug, Eq, PartialEq)]
pub enum RegisterTokenizerError {
    SelectFts5Failed,
    Fts5ApiNul,
    Fts5ApiVersionTooLow,
    Fts5xCreateTokenizerV2Nul,
    Fts5xCreateTokenizerFailed(i32),
}

impl std::fmt::Display for RegisterTokenizerError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            RegisterTokenizerError::SelectFts5Failed => {
                write!(f, "SELECT fts5(?1) failed.")
            }
            RegisterTokenizerError::Fts5ApiNul => {
                write!(f, "Could not get fts5 api.")
            }
            RegisterTokenizerError::Fts5ApiVersionTooLow => {
                write!(f, "The version of fts5 api is too low.")
            }
            RegisterTokenizerError::Fts5xCreateTokenizerV2Nul => {
                write!(f, "Fts5 api xCreateTokenizer_v2 ptr is null.")
            }
            RegisterTokenizerError::Fts5xCreateTokenizerFailed(rc) => {
                write!(
                    f,
                    "Fts5 xCreateTokenizer failed, the error flag when sqlite returned is {rc}."
                )
            }
        }
    }
}

impl std::error::Error for RegisterTokenizerError {}

unsafe fn get_fts5_api(db: &Connection) -> Result<*mut fts5_api, RegisterTokenizerError> {
    let dbp = unsafe { db.handle() };
    let mut api: *mut fts5_api = std::ptr::null_mut();
    let mut stmt: *mut sqlite3_stmt = std::ptr::null_mut();
    const FTS5_QUERY_STATEMENT: &CStr = c"SELECT fts5(?1)";
    const FTS5_QUERY_STATEMENT_LEN: c_int = FTS5_QUERY_STATEMENT.count_bytes() as c_int;
    unsafe {
        if sqlite3_prepare_v3(
            dbp,
            FTS5_QUERY_STATEMENT.as_ptr(),
            FTS5_QUERY_STATEMENT_LEN,
            0,
            &raw mut stmt,
            std::ptr::null_mut(),
        ) != SQLITE_OK
            || stmt.is_null()
        {
            return Err(RegisterTokenizerError::SelectFts5Failed);
        }
        let bind_rc = sqlite3_bind_pointer(
            stmt,
            1,
            (&raw mut api).cast::<c_void>(),
            c"fts5_api_ptr".as_ptr(),
            None,
        );
        if bind_rc != SQLITE_OK {
            sqlite3_finalize(stmt);
            return Err(RegisterTokenizerError::SelectFts5Failed);
        }
        // M8a: step 失败（非 ROW/DONE）必须上抛——否则 api 指针可能未置位，
        // 静默继续会拿 null/陈旧指针注册 tokenizer。
        let step_rc = sqlite3_step(stmt);
        sqlite3_finalize(stmt);
        if step_rc != SQLITE_ROW && step_rc != SQLITE_DONE {
            return Err(RegisterTokenizerError::SelectFts5Failed);
        }
    }
    if api.is_null() {
        return Err(RegisterTokenizerError::Fts5ApiNul);
    }
    Ok(api)
}

unsafe fn register_tokenizer_on_api<T: Tokenizer>(
    api: *mut fts5_api,
    global_data: T::Global,
) -> Result<(), RegisterTokenizerError> {
    if api.is_null() {
        return Err(RegisterTokenizerError::Fts5ApiNul);
    }
    if unsafe { (*api).iVersion } < FTS5_API_VERSION {
        return Err(RegisterTokenizerError::Fts5ApiVersionTooLow);
    }
    let create_fn = unsafe { (*api).xCreateTokenizer_v2 }
        .ok_or(RegisterTokenizerError::Fts5xCreateTokenizerV2Nul)?;

    let global_ptr = Box::into_raw(Box::new(global_data));
    let mut tokenizer_v2 = fts5_tokenizer_v2 {
        iVersion: FTS5_TOKENIZER_VERSION,
        xCreate: Some(x_create::<T>),
        xDelete: Some(x_delete::<T>),
        xTokenize: Some(x_tokenize::<T>),
    };

    // Once `global_ptr` enters `xCreateTokenizer_v2`, SQLite FTS5's
    // `fts5CreateTokenizer` invokes `xDestroy` even on non-OK return codes,
    // so do not call `Box::from_raw(global_ptr)` on error here.
    let rc = unsafe {
        create_fn(
            api,
            T::name().as_ptr(),
            global_ptr.cast::<c_void>(),
            &raw mut tokenizer_v2,
            Some(x_destroy::<T>),
        )
    };
    if rc != SQLITE_OK {
        return Err(RegisterTokenizerError::Fts5xCreateTokenizerFailed(rc));
    }
    Ok(())
}

/// Registers an FTS5 v2 tokenizer on the given SQLite connection.
pub fn register_tokenizer<T: Tokenizer>(
    db: &Connection,
    global_data: T::Global,
) -> Result<(), RegisterTokenizerError> {
    unsafe {
        let api: *mut fts5_api = get_fts5_api(db)?;
        register_tokenizer_on_api::<T>(api, global_data)
    }
}

#[cfg(test)]
#[path = "fts5_ffi_tests.rs"]
mod tests;
