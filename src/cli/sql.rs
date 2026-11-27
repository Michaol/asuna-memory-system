use crate::index::db::Db;

pub(crate) fn format_sql_value(v: rusqlite::types::ValueRef<'_>) -> String {
    match v {
        rusqlite::types::ValueRef::Null => "NULL".to_string(),
        rusqlite::types::ValueRef::Integer(n) => n.to_string(),
        rusqlite::types::ValueRef::Real(f) => f.to_string(),
        rusqlite::types::ValueRef::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        rusqlite::types::ValueRef::Blob(bytes) => format!("<blob {} bytes>", bytes.len()),
    }
}

/// M7: 剥掉 SQL 前导的空白、`--` 行注释、`/* */` 块注释与左括号，供首 token 判定。
fn strip_sql_prefix(q: &str) -> &str {
    let mut s = q.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            match rest.find('\n') {
                Some(i) => s = rest[i + 1..].trim_start(),
                None => return "",
            }
        } else if let Some(rest) = s.strip_prefix("/*") {
            match rest.find("*/") {
                Some(i) => s = rest[i + 2..].trim_start(),
                None => return "",
            }
        } else if let Some(rest) = s.strip_prefix('(') {
            s = rest.trim_start();
        } else {
            return s;
        }
    }
}

/// M3: 取前导 SQL 关键字——必须以字母数字开头。前导引号/标点（如 `"'select' 1"`）
/// 不是关键字，不得剥掉后误判为 SELECT 放行。
fn leading_sql_keyword(q: &str) -> String {
    q.chars()
        .take_while(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_uppercase()
}

/// C2: 词法扫描——跳过 `'…'` / `"…"` / `` `…` `` 引用（双写转义）、`[…]` 方括号
/// 标识符与 `--` / `/* */` 注释，收集引用/注释外的语句分隔符 `;` 位置。
fn top_level_semicolons(q: &str) -> Vec<usize> {
    let mut seps = Vec::new();
    let mut chars = q.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '\'' | '"' | '`' => {
                while let Some((_, n)) = chars.next() {
                    if n == c {
                        if chars.peek().is_some_and(|&(_, p)| p == c) {
                            chars.next(); // 双写转义（'' / "" / ``），仍在引用内
                        } else {
                            break;
                        }
                    }
                }
            }
            '[' => {
                for (_, n) in chars.by_ref() {
                    if n == ']' {
                        break;
                    }
                }
            }
            '-' if chars.peek().is_some_and(|&(_, n)| n == '-') => {
                for (_, n) in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek().is_some_and(|&(_, n)| n == '*') => {
                chars.next();
                let mut prev = '\0';
                for (_, n) in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            ';' => seps.push(i),
            _ => {}
        }
    }
    seps
}

/// C2: 尾随 `;` 之后只允许空白与注释（不得有其他语句/记号）。
fn trailing_only_whitespace_or_comments(mut s: &str) -> bool {
    loop {
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix("--") {
            match rest.find('\n') {
                Some(i) => s = &rest[i + 1..],
                None => return true,
            }
        } else if let Some(rest) = s.strip_prefix("/*") {
            match rest.find("*/") {
                Some(i) => s = &rest[i + 2..],
                None => return true,
            }
        } else {
            return s.is_empty();
        }
    }
}

/// M7/C2: 判断 SQL 是否含多条语句——单个可选尾随 `;`（其后仅空白/注释）放行；
/// 任何额外 `;` 或尾随语句都拒绝。引用/注释内的 `;` 不是分隔符。
fn has_statement_separator_outside_quotes(q: &str) -> bool {
    let seps = top_level_semicolons(q);
    match seps.as_slice() {
        [] => false,
        [i] => !trailing_only_whitespace_or_comments(&q[i + 1..]),
        _ => true,
    }
}

/// 只读 SQL 查询（在只读连接 + SQLite Authorizer 下执行，tokenize_zh UDF 可用）
pub(crate) fn cmd_sql(db: &Db, query: &str) -> anyhow::Result<()> {
    // M7: 先剥前导注释/空白/括号再取首 token——否则 "/* 审计 */ SELECT …"、
    // "-- 注释\nSELECT …"、"(SELECT 1) UNION …" 这些纯只读查询被误拒为"安全"错误。
    // 真正的写拦截由只读连接 + Authorizer 保证，本预筛只是友好提示。
    // M3: 首 token 必须是**前导**字母数字串——`"'select' 1"` 前导是引号，取到空串
    // 被拒，不再剥掉引号/标点后误判为 SELECT。
    let head = strip_sql_prefix(query);
    let first_token = leading_sql_keyword(head);
    if !matches!(
        first_token.as_str(),
        "SELECT" | "PRAGMA" | "EXPLAIN" | "WITH"
    ) {
        anyhow::bail!(
            "safety: sql subcommand only allows read queries (SELECT/PRAGMA/EXPLAIN/WITH)"
        );
    }
    // M7: prepare 只编译首语句，尾语句会被静默丢弃。仅在**引号外**出现 ';' 时拒绝
    // ——字符串字面量里的 ';'（如 LIKE '%;%'）是合法单语句，不得误拒。
    if has_statement_separator_outside_quotes(query) {
        anyhow::bail!("sql subcommand 只支持单条语句（检测到多条；尾语句会被静默忽略）");
    }

    let mut stmt = db.conn().prepare(query)?;
    let cols: Vec<String> = stmt.column_names().map(ToString::to_string).collect();
    let col_count = cols.len();
    println!("{}", cols.join("\t"));
    println!("{}", "-".repeat(col_count * 20));

    let rows = stmt.query_map([], |row| {
        let mut vals = Vec::with_capacity(col_count);
        for i in 0..col_count {
            vals.push(format_sql_value(row.get_ref(i)?));
        }
        Ok(vals)
    })?;

    let mut count = 0;
    for row in rows {
        let vals = row?;
        println!("{}", vals.join("\t"));
        count += 1;
    }
    println!("\n{} rows", count);
    Ok(())
}
