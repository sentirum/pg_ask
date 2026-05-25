//! Render `ColumnRow`s into the compact form we ship in the system prompt.
//!
//! Format (deliberately stable so prompt diffs stay reviewable):
//!
//! ```text
//! TABLE public.orders
//!   id uuid NOT NULL
//!   user_id uuid NOT NULL  -- fk: users.id
//!   total numeric
//!   created_at timestamptz NOT NULL
//!
//! TABLE public.users
//!   id uuid NOT NULL
//!   email text NOT NULL  -- sensitive
//! ```
//!
//! v0.3 will add a `render_compact` for the token-budget mode (tables-only
//! listing) and a `render_describe` for the `describe_table` tool.

use super::introspect::ColumnRow;

pub fn render(rows: &[ColumnRow]) -> String {
    if rows.is_empty() {
        return "(no user-visible tables found)".into();
    }

    let mut out = String::new();
    let mut current_table: Option<(&str, &str)> = None;

    for r in rows {
        let key = (r.schema.as_str(), r.table.as_str());
        if current_table != Some(key) {
            if current_table.is_some() {
                out.push('\n');
            }
            out.push_str("TABLE ");
            out.push_str(r.schema.as_str());
            out.push('.');
            out.push_str(r.table.as_str());
            out.push('\n');
            current_table = Some(key);
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
