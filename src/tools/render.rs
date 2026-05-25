//! Shared rendering helpers for the table-shaped tools (`sql_query`,
//! `sample_table`, `user_defined`).
//!
//! ## Why this exists
//!
//! Before v0.5.2 each tool reached into the SPI tuple table with
//! `row.get_datum_by_ordinal(c).value::<String>()`. In pgrx 0.18 the
//! `value::<T>()` call is **type-strict** — passing `String` to an `int4`
//! datum returns `None`, not the text representation. The previous code
//! then collapsed `None` to the literal string `"NULL"`, which meant any
//! query touching numeric, timestamp, uuid, jsonb, etc. columns returned
//! a wall of `"NULL"`s back to the model. That broke the core happy path.
//!
//! ## The fix
//!
//! We let Postgres do the work: wrap the user's query in
//!
//! ```sql
//! SELECT row_to_json(t)::text FROM (<orig>) t LIMIT <cap + 1>
//! ```
//!
//! and parse each row as JSON. This gives us:
//!
//! * Native text serialization for every PG type, including custom
//!   domains and arrays, with NULL handled correctly.
//! * A hard `LIMIT cap + 1` enforced *in SQL* (the `+1` lets us detect
//!   truncation without materialising the rest).
//! * One code path shared across the three tools — no more 3× copies of
//!   the iteration / sensitive-column / table-rendering logic.
//! * Zero `unsafe` (we still go through pgrx's safe SPI surface — the
//!   only datum type we extract is `String`, which is what
//!   `row_to_json(...)::text` produces).
//!
//! Column order is preserved because `serde_json` is built with the
//! `preserve_order` feature in `Cargo.toml`.

use pgrx::prelude::*;
use serde_json::Value;

/// Width clamp per cell before rendering. Long blobs (jsonb, base64,
/// arrays) get truncated with an ellipsis so we don't blow the model's
/// context window.
pub const MAX_CELL_CHARS: usize = 500;

/// Result of `run_to_table` — split so callers (in particular `sql_query`)
/// can audit the actual row count separately from the rendered text.
///
/// `row_count` / `truncated` are unused today; H3 (audit-row update) and
/// future per-tool telemetry will read them. Keeping them on the struct
/// now avoids a churny signature change when those land.
#[allow(dead_code)]
pub struct RenderedTable {
    pub text: String,
    pub row_count: usize,
    pub truncated: bool,
}

/// Run `query` under the active SPI session and return a pretty
/// printed table, plus metadata.
///
/// `sensitive` are column-name patterns to mask with `<redacted>`.
/// Patterns may be a bare column name (`password`) or a dotted suffix
/// (`users.password`, `public.users.password`). With the JSON wrapper
/// we only ever see the bare alias the user/PG chose for the SELECT
/// list, so dotted patterns will only match if the operator pre-aliased
/// the column (e.g. `SELECT users.password AS users_password`). Callers
/// that want full FQN matching should pre-process columns in SQL.
///
/// The query is wrapped — callers MUST NOT pre-wrap it. The wrapper:
///
/// 1. Adds `LIMIT max_rows + 1` so we can detect truncation cheaply.
/// 2. Pivots to `row_to_json(t)::text` so we get a single `text`
///    column we can extract with pgrx's safe SPI surface.
///
/// `query` must be a single self-contained `SELECT` (the SQL guard
/// already enforces this upstream).
pub fn run_to_table(
    query: &str,
    max_rows: usize,
    sensitive: &[String],
) -> Result<RenderedTable, String> {
    // The wrapper is parametrised by the cap; we never trust the model
    // to add `LIMIT` itself. Note we still let the inner query's own
    // `LIMIT` apply first if smaller — `LIMIT` composes by intersection
    // when the inner is a subquery, which is what we want.
    let cap_plus_one = max_rows.saturating_add(1) as i64;
    let wrapped = wrap_with_cap(query);

    Spi::connect(|client| -> Result<RenderedTable, String> {
        let tuptable = client
            .select(&wrapped, Some(cap_plus_one), &[cap_plus_one.into()])
            .map_err(|e| e.to_string())?;

        let (json_rows, truncated) = parse_json_rows(tuptable, max_rows)?;
        let (text, row_count) = format_table(&json_rows, sensitive, truncated, max_rows);

        Ok(RenderedTable {
            text,
            row_count,
            truncated,
        })
    })
}

/// Wrap an arbitrary `SELECT` into the `row_to_json` shape we use for
/// safe text extraction. Callers can run the wrapped string themselves
/// (with bind args) and then call `parse_json_rows` + `pivot_rows` +
/// `apply_sensitive` + `render_table` to get the same output as
/// `run_to_table`. This is the seam `user_defined` uses to thread
/// model-supplied bind arguments through without exposing them to SQL
/// concatenation (C4).
///
/// The wrapper takes one bound parameter — the row cap (`$1` from the
/// caller's perspective). User-supplied placeholders should be numbered
/// `$2..$N` so they don't collide.
pub fn wrap_with_cap(inner: &str) -> String {
    format!(
        "SELECT row_to_json(_pg_ask_t)::text \
         FROM ({inner}) AS _pg_ask_t \
         LIMIT $1"
    )
}

/// Parse the single-column text result of a `wrap_with_cap` query into
/// JSON values, honouring the row cap with the over-fetch trick.
///
/// Each row's datum is a `text` containing the JSON encoding of the
/// original row, so the only datum type we ever ask SPI for is
/// `String` — no type-strict surprises (C2).
pub fn parse_json_rows(
    tuptable: pgrx::spi::SpiTupleTable,
    max_rows: usize,
) -> Result<(Vec<Value>, bool), String> {
    let mut json_rows: Vec<Value> = Vec::new();
    let mut truncated = false;
    for (idx, row) in tuptable.into_iter().enumerate() {
        if idx >= max_rows {
            truncated = true;
            break;
        }
        let cell: Option<String> = row
            .get_datum_by_ordinal(1)
            .ok()
            .and_then(|d| d.value::<String>().ok().flatten());
        let json_text = cell.unwrap_or_else(|| "null".to_string());
        let parsed: Value = serde_json::from_str(&json_text)
            .map_err(|e| format!("internal: row_to_json parse failed: {e}"))?;
        json_rows.push(parsed);
    }
    Ok((json_rows, truncated))
}

/// Format pivoted rows + caps into the text table we send to the model.
/// Combines pivot → sensitive masking → layout. Exposed for callers
/// that ran the wrapped SQL themselves (see `user_defined`).
pub fn format_table(
    json_rows: &[Value],
    sensitive: &[String],
    truncated: bool,
    cap: usize,
) -> (String, usize) {
    let (columns, cells) = pivot_rows(json_rows);
    let row_count = cells.len();
    let masked = apply_sensitive(&columns, cells, sensitive);
    let text = render_table(&columns, &masked, truncated, cap);
    (text, row_count)
}

/// Pull column names from the first row's object (key order = SELECT
/// list order, courtesy of `preserve_order`) and flatten each row to a
/// `Vec<String>` aligned to those columns.
///
/// Tolerates heterogeneous rows: any column missing from a given row is
/// rendered as `NULL`. In practice every row has the same shape because
/// they all come from one `SELECT`, but defensive code is cheap.
fn pivot_rows(rows: &[Value]) -> (Vec<String>, Vec<Vec<String>>) {
    if rows.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let columns: Vec<String> = match rows[0].as_object() {
        Some(obj) => obj.keys().cloned().collect(),
        // `SELECT NULL::record` would land here.
        None => vec!["?column?".to_string()],
    };
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|col| {
                    row.get(col)
                        .map(value_to_cell)
                        .unwrap_or_else(|| "NULL".to_string())
                })
                .collect()
        })
        .collect();
    (columns, cells)
}

/// Turn a JSON value into the compact form we want to show the model.
///
/// * `null`         → `NULL` (matches psql's display, distinguishes from
///                    the empty string).
/// * strings        → the raw string (no surrounding quotes — that's how
///                    psql renders text columns too).
/// * everything else → its JSON encoding (numbers as decimals, arrays
///                    and objects in their JSON form).
fn value_to_cell(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::String(s) => truncate_cell(s),
        other => truncate_cell(&other.to_string()),
    }
}

fn apply_sensitive(
    columns: &[String],
    cells: Vec<Vec<String>>,
    sensitive: &[String],
) -> Vec<Vec<String>> {
    if sensitive.is_empty() {
        return cells;
    }
    let mask: Vec<bool> = columns.iter().map(|c| is_sensitive_col(c, sensitive)).collect();
    cells
        .into_iter()
        .map(|row| {
            row.into_iter()
                .enumerate()
                .map(|(i, v)| {
                    if mask.get(i).copied().unwrap_or(false) {
                        "<redacted>".to_string()
                    } else {
                        v
                    }
                })
                .collect()
        })
        .collect()
}

/// Match column against an exact-name or dotted-suffix pattern.
///
/// NOTE: with the `row_to_json` wrapper the column names are the
/// aliases from the inner SELECT, not fully-qualified names. So a
/// pattern like `users.password` will only match if the caller used
/// `SELECT users.password ...` with no rename, which Postgres
/// normalises to just `password`. In practice users should specify
/// patterns by the bare alias they expect to see; FQN-style patterns
/// are kept for forward compatibility / `sample_table` reuse.
fn is_sensitive_col(col: &str, patterns: &[String]) -> bool {
    patterns
        .iter()
        .any(|p| col.eq_ignore_ascii_case(p) || col.to_ascii_lowercase().ends_with(&format!(".{}", p.to_ascii_lowercase())))
}

fn truncate_cell(s: &str) -> String {
    // Fast path: scan up to MAX_CELL_CHARS+1 chars so we don't pay
    // O(len) for huge cells just to discard them.
    let mut count = 0usize;
    for (i, _) in s.char_indices() {
        count += 1;
        if count > MAX_CELL_CHARS {
            // `i` is the byte offset of the (MAX_CELL_CHARS + 1)-th
            // char, so slicing up to `i` is on a char boundary.
            return format!("{}…", &s[..i]);
        }
    }
    s.to_string()
}

fn render_table(cols: &[String], rows: &[Vec<String>], truncated: bool, cap: usize) -> String {
    if rows.is_empty() {
        return format!("(0 rows)\ncolumns: {}", cols.join(", "));
    }
    let widths: Vec<usize> = cols
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let max_cell = rows
                .iter()
                .map(|r| r.get(i).map(|s| s.chars().count()).unwrap_or(0))
                .max()
                .unwrap_or(0);
            name.chars().count().max(max_cell)
        })
        .collect();

    let mut out = String::new();
    fmt_row(&mut out, cols.iter().map(String::as_str), &widths);
    fmt_sep(&mut out, &widths);
    for row in rows {
        fmt_row(&mut out, row.iter().map(String::as_str), &widths);
    }
    out.push_str(&format!("\n({} rows)", rows.len()));
    if truncated {
        out.push_str(&format!(" — truncated at {cap}"));
    }
    out
}

fn fmt_row<'a, I: Iterator<Item = &'a str>>(out: &mut String, cells: I, widths: &[usize]) {
    let parts: Vec<String> = cells
        .zip(widths.iter())
        .map(|(c, w)| format!("{:<width$}", c, width = w))
        .collect();
    out.push_str(&parts.join(" | "));
    out.push('\n');
}

fn fmt_sep(out: &mut String, widths: &[usize]) {
    let parts: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    out.push_str(&parts.join("-+-"));
    out.push('\n');
}

// ---------------------------------------------------------------------------
// Tests — pure functions only (no SPI). The SPI-touching `run_to_table`
// is covered by #[pg_test] in `tests/`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pivot_preserves_column_order() {
        let rows = vec![json!({"id": 1, "name": "ada", "joined": "2024-01-01"})];
        let (cols, cells) = pivot_rows(&rows);
        assert_eq!(cols, vec!["id", "name", "joined"]);
        assert_eq!(cells, vec![vec!["1", "ada", "2024-01-01"]]);
    }

    #[test]
    fn null_renders_as_literal_string() {
        let rows = vec![json!({"a": null, "b": 0})];
        let (_, cells) = pivot_rows(&rows);
        assert_eq!(cells[0], vec!["NULL", "0"]);
    }

    #[test]
    fn complex_types_serialise_as_json() {
        let rows = vec![json!({
            "ints": [1, 2, 3],
            "obj": {"k": "v"},
            "float": 3.14,
            "bool": true,
        })];
        let (_, cells) = pivot_rows(&rows);
        assert_eq!(cells[0][0], "[1,2,3]");
        assert!(cells[0][1].contains("\"k\""));
        assert_eq!(cells[0][2], "3.14");
        assert_eq!(cells[0][3], "true");
    }

    #[test]
    fn sensitive_redaction_matches_by_name_and_suffix() {
        let cells = vec![vec!["plain".into(), "secret".into(), "shhh".into()]];
        let cols = vec!["name".to_string(), "password".to_string(), "users.api_key".to_string()];
        let masked = apply_sensitive(
            &cols,
            cells,
            &["password".to_string(), "api_key".to_string()],
        );
        assert_eq!(masked[0], vec!["plain", "<redacted>", "<redacted>"]);
    }

    #[test]
    fn sensitive_match_is_case_insensitive() {
        let cells = vec![vec!["secret".into()]];
        let masked = apply_sensitive(
            &["Password".to_string()],
            cells,
            &["password".to_string()],
        );
        assert_eq!(masked[0], vec!["<redacted>"]);
    }

    #[test]
    fn truncate_cell_respects_char_boundaries() {
        // String of MAX_CELL_CHARS + 50 multi-byte codepoints (CJK char = 3 bytes UTF-8).
        // Exercises the char_indices boundary-respecting truncation path.
        let s: String = std::iter::repeat('文').take(MAX_CELL_CHARS + 50).collect();
        let cut = truncate_cell(&s);
        assert!(cut.ends_with('…'));
        // chars().count() is MAX_CELL_CHARS + 1 (the ellipsis).
        assert_eq!(cut.chars().count(), MAX_CELL_CHARS + 1);
    }

    #[test]
    fn render_empty_keeps_column_header() {
        let out = render_table(&["a".to_string(), "b".to_string()], &[], false, 100);
        assert_eq!(out, "(0 rows)\ncolumns: a, b");
    }

    #[test]
    fn render_truncated_marks_cap() {
        let cols = vec!["x".to_string()];
        let rows = vec![vec!["1".to_string()], vec!["2".to_string()]];
        let out = render_table(&cols, &rows, true, 2);
        assert!(out.contains("truncated at 2"));
    }
}
