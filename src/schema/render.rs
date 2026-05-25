//! Render `ColumnRow`s into the textual form we ship in the system prompt.
//!
//! Two stable formats so prompt diffs stay reviewable:
//!
//! **Full** — every column of every table, with table- and column-level
//! comments folded inline:
//!
//! ```text
//! TABLE public.orders -- customer orders
//!   id uuid NOT NULL
//!   user_id uuid NOT NULL  -- references users.id
//!   total numeric
//!   created_at timestamptz NOT NULL
//!
//! TABLE public.users
//!   id uuid NOT NULL
//!   email text NOT NULL  -- sensitive
//! ```
//!
//! **Compact** — tables-only listing, used when the full render would blow
//! the operator's `pg_ask.schema_char_budget`. The compact prompt invites
//! the model to call `describe_table` for column detail:
//!
//! ```text
//! TABLES (use describe_table for columns):
//!   public.orders  (12 columns) -- customer orders
//!   public.users   (5 columns)
//!   ...
//! ```

use super::introspect::{ColumnRow, TableKey};
use std::collections::HashMap;

pub fn render_full(rows: &[ColumnRow], table_comments: &[(TableKey, String)]) -> String {
    if rows.is_empty() {
        return "(no user-visible tables found)".into();
    }
    let comment_lookup: HashMap<TableKey, &str> = table_comments
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str()))
        .collect();

    let mut out = String::new();
    let mut current: Option<(&str, &str)> = None;

    for r in rows {
        let key = (r.schema.as_str(), r.table.as_str());
        if current != Some(key) {
            if current.is_some() {
                out.push('\n');
            }
            out.push_str("TABLE ");
            out.push_str(r.schema.as_str());
            out.push('.');
            out.push_str(r.table.as_str());
            let tk = TableKey {
                schema: r.schema.clone(),
                table: r.table.clone(),
            };
            if let Some(c) = comment_lookup.get(&tk) {
                out.push_str(" -- ");
                out.push_str(c);
            }
            out.push('\n');
            current = Some(key);
        }
        out.push_str("  ");
        out.push_str(&r.column);
        out.push(' ');
        out.push_str(&r.data_type);
        if r.not_null {
            out.push_str(" NOT NULL");
        }
        if !r.comment.is_empty() {
            out.push_str("  -- ");
            out.push_str(&r.comment);
        }
        out.push('\n');
    }
    out
}

pub fn render_compact(rows: &[ColumnRow], table_comments: &[(TableKey, String)]) -> String {
    if rows.is_empty() {
        return "(no user-visible tables found)".into();
    }

    // Group by (schema, table) preserving the order of first appearance —
    // introspect.rs already sorts by schema, table, attnum.
    #[derive(Default)]
    struct Acc {
        order: Vec<TableKey>,
        col_counts: HashMap<TableKey, usize>,
    }
    let mut acc = Acc::default();

    for r in rows {
        let key = TableKey {
            schema: r.schema.clone(),
            table: r.table.clone(),
        };
        let entry = acc.col_counts.entry(key.clone()).or_insert(0);
        if *entry == 0 {
            acc.order.push(key);
        }
        *entry += 1;
    }

    let comment_lookup: HashMap<TableKey, &str> = table_comments
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str()))
        .collect();

    // Column width for the "schema.table" segment so the (N columns) suffix
    // lines up — a tiny ergonomic touch that makes the prompt readable when
    // the operator inspects it.
    let max_name_width = acc
        .order
        .iter()
        .map(|k| k.schema.chars().count() + 1 + k.table.chars().count())
        .max()
        .unwrap_or(0);

    let mut out = String::new();
    out.push_str("TABLES (use describe_table for columns):\n");
    for key in &acc.order {
        let qname = format!("{}.{}", key.schema, key.table);
        let count = acc.col_counts[key];
        let _ = max_name_width;
        out.push_str("  ");
        out.push_str(&format!("{:<width$}", qname, width = max_name_width));
        out.push_str(&format!("  ({count} columns)"));
        if let Some(c) = comment_lookup.get(key) {
            out.push_str(" -- ");
            out.push_str(c);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(schema: &str, table: &str, name: &str, ty: &str) -> ColumnRow {
        ColumnRow {
            schema: schema.into(),
            table: table.into(),
            column: name.into(),
            data_type: ty.into(),
            not_null: false,
            comment: String::new(),
        }
    }

    #[test]
    fn full_groups_columns_under_table() {
        let rows = vec![
            col("public", "users", "id", "uuid"),
            col("public", "users", "email", "text"),
            col("public", "orders", "id", "uuid"),
        ];
        let out = render_full(&rows, &[]);
        assert!(out.contains("TABLE public.users"));
        assert!(out.contains("  id uuid"));
        assert!(out.contains("  email text"));
        assert!(out.contains("TABLE public.orders"));
    }

    #[test]
    fn full_inlines_table_comment() {
        let rows = vec![col("public", "users", "id", "uuid")];
        let comments = vec![(
            TableKey {
                schema: "public".into(),
                table: "users".into(),
            },
            "all customers".into(),
        )];
        let out = render_full(&rows, &comments);
        assert!(out.contains("TABLE public.users -- all customers"));
    }

    #[test]
    fn compact_lists_tables_with_column_counts() {
        let rows = vec![
            col("public", "users", "id", "uuid"),
            col("public", "users", "email", "text"),
            col("public", "orders", "id", "uuid"),
        ];
        let out = render_compact(&rows, &[]);
        assert!(out.starts_with("TABLES (use describe_table for columns):\n"));
        assert!(out.contains("public.users"));
        assert!(out.contains("(2 columns)"));
        assert!(out.contains("public.orders"));
        assert!(out.contains("(1 columns)"));
    }

    #[test]
    fn empty_returns_placeholder() {
        assert_eq!(render_full(&[], &[]), "(no user-visible tables found)");
        assert_eq!(render_compact(&[], &[]), "(no user-visible tables found)");
    }
}
