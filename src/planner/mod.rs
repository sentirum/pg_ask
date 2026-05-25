//! Query previewer.
//!
//! Takes a model-generated SQL string, validates it, asks Postgres to
//! `EXPLAIN (FORMAT JSON)` it under a readonly sub-transaction, then
//! distils the plan into operator-friendly columns:
//!
//! * `generated_sql` — the SQL after stripping any leading `EXPLAIN`
//!   the model may have prepended (we do the explaining; the model
//!   should only produce the underlying query).
//! * `est_rows`      — root-node `Plan Rows` from the planner.
//! * `tables`        — every `schema.table` the plan touches.
//! * `warnings`      — heuristic risk notes (Seq Scan on a wide table,
//!   filter on every row, no `LIMIT`, large estimated row count, …).
//!
//! Never executes the underlying query. Even `EXPLAIN` runs without
//! `ANALYZE`, and we wrap it in `transaction_read_only = on` so a
//! stray write inside a function would still be blocked.

mod analysis;
mod explain;

use crate::infra::errors::{AskError, Result};
use crate::sql_guard::{self, GuardMode};

/// One result row of `pg_ask.preview`.
#[derive(Debug)]
pub struct PreviewRow {
    pub generated_sql: String,
    pub est_rows: i64,
    pub tables: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn preview(raw_sql: &str) -> Result<PreviewRow> {
    // 1. If the model included a leading EXPLAIN, strip it. The whole point
    //    of preview() is that *we* are the ones doing the EXPLAIN; letting
    //    the model nest `EXPLAIN ANALYZE` past the guard would execute the
    //    inner query as a side effect.
    let cleaned = strip_leading_explain(raw_sql.trim());

    // 2. Validate against the same rules sql_query uses in readonly mode.
    //    Anything the agent can run, preview() can preview — and nothing else.
    let validated = sql_guard::validate(&cleaned, GuardMode::Readonly)?;

    // 3. EXPLAIN it under a readonly sub-tx and parse the JSON.
    let plan_json = explain::run(validated.as_str())?;

    // 4. Distil into the operator-facing summary.
    let summary = analysis::summarize(&plan_json)
        .ok_or_else(|| AskError::Sql("EXPLAIN returned an unexpected JSON shape".into()))?;

    Ok(PreviewRow {
        generated_sql: cleaned,
        est_rows: summary.est_rows,
        tables: summary.tables,
        warnings: summary.warnings,
    })
}

/// Remove leading `EXPLAIN [(...)] [ANALYZE] [VERBOSE] …` so we control the
/// EXPLAIN ourselves. We only strip the *leading* keyword sequence; once a
/// non-EXPLAIN token shows up we stop. Operates on a trimmed input.
fn strip_leading_explain(sql: &str) -> String {
    let lower = sql.to_ascii_lowercase();
    if !lower.starts_with("explain") {
        return sql.to_string();
    }

    let bytes = sql.as_bytes();
    let mut i = "explain".len();

    // Optional `( ... )` options list.
    i = skip_ws(bytes, i);
    if bytes.get(i) == Some(&b'(') {
        // walk to matching close paren (no nested parens expected in EXPLAIN options)
        let mut depth = 1;
        i += 1;
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
    } else {
        // Bareword options: ANALYZE, VERBOSE, BUFFERS, … chained until the verb.
        loop {
            i = skip_ws(bytes, i);
            let word_start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
                i += 1;
            }
            if i == word_start {
                break;
            }
            let word = lower[word_start..i].to_string();
            // Stop as soon as we hit a real verb (SELECT/WITH/TABLE/VALUES).
            if matches!(word.as_str(), "select" | "with" | "table" | "values") {
                i = word_start;
                break;
            }
            // Otherwise it was an EXPLAIN option — keep going.
        }
    }

    sql[i..].trim_start().to_string()
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

#[cfg(test)]
mod strip_tests {
    use super::strip_leading_explain;

    #[test]
    fn passthrough_when_no_explain() {
        assert_eq!(strip_leading_explain("SELECT 1"), "SELECT 1");
        assert_eq!(strip_leading_explain("with x as (select 1) select * from x"),
                   "with x as (select 1) select * from x");
    }

    #[test]
    fn strips_bareword_options() {
        assert_eq!(strip_leading_explain("EXPLAIN SELECT 1"), "SELECT 1");
        assert_eq!(strip_leading_explain("EXPLAIN ANALYZE SELECT 1"), "SELECT 1");
        assert_eq!(strip_leading_explain("explain analyze verbose buffers select * from t"),
                   "select * from t");
    }

    #[test]
    fn strips_parenthesised_options() {
        assert_eq!(strip_leading_explain("EXPLAIN (ANALYZE, BUFFERS) SELECT 1"), "SELECT 1");
        assert_eq!(strip_leading_explain("EXPLAIN (FORMAT JSON) SELECT * FROM t"),
                   "SELECT * FROM t");
    }
}
