//! User-defined tools — SQL snippets registered by operators via
//! `ask.register_tool(name, spec, body)`.
//!
//! ## Argument interpolation (C4 in the v0.5.2 review)
//!
//! At invocation time the tool's jsonb arguments are substituted into
//! `{{key}}` placeholders in the body string. Before v0.5.2 we did this
//! by raw string substitution (`out.replace("{{x}}", arg_value)`),
//! which meant a model-supplied argument like `0; DROP TABLE _config; --`
//! ended up concatenated straight into the SQL. The operator's tool
//! body was trusted but the model's arguments were not — and we trusted
//! both.
//!
//! The fix: scan the body for `{{key}}` markers, replace each with a
//! numbered placeholder (`$2, $3, …` — `$1` is reserved for the row
//! cap), and pass the corresponding jsonb value as a bind parameter to
//! the SQL engine. The body author keeps the same `{{key}}` syntax;
//! the value never participates in SQL parsing.
//!
//! ## Type mapping
//!
//! JSON → Postgres:
//!  * `string`  → `text`
//!  * `integer` → `int8` (JSON has no int/float distinction, but
//!                serde_json keeps the original lexeme; we route
//!                values without a fractional part to int8 so they
//!                cast cleanly to numeric column targets)
//!  * `number`  → `float8`
//!  * `bool`    → `bool`
//!  * `null`    → typed NULL (we emit `NULL::text` so the type is
//!                inferable in plain interpolation contexts)
//!  * `array` / `object` → `jsonb`
//!
//! ## Operator responsibility
//!
//! There is no sql_guard on user-defined tool bodies because the operator
//! explicitly opted in. The body should still be written defensively (e.g.
//! `WHERE col = {{key}}` rather than `WHERE col IN ({{key}})` if the model
//! might pass a list and you weren't expecting jsonb).
//!
//! ## HP1 (Gemini v0.5.2 review item 1.2): isolation + timeout + readonly
//!
//! Pre-v0.5.3 the body ran via a plain `Spi::connect` with no
//! subtransaction, no per-call GUCs, and no statement_timeout. Three
//! observable problems:
//!
//! 1. A Postgres ERROR inside the body (typo, missing table, divide
//!    by zero, anything) poisoned the surrounding `ask.ask()`
//!    transaction — every subsequent SPI call in the same agent
//!    loop then failed with `current transaction is aborted,
//!    commands ignored`.
//! 2. With `pg_ask.readonly = on` the operator-blessed body could
//!    still issue DML (the readonly flag had to be flipped on by the
//!    enclosing `ask.ask()` to stop it, but for `ask.chat()` and the
//!    EXPLAIN path it wasn't).
//! 3. No `SET LOCAL statement_timeout` meant a runaway body could
//!    sit on the backend until the client connection closed.
//!
//! Fix: mirror what `sql_query` and `sample_table` already do —
//! wrap `run_planned` in `infra::subtxn::run_in_subtransaction` and
//! call `apply_per_call_gucs(readonly, statement_timeout_ms)` from
//! inside the subtxn so the GUCs revert when the subtxn releases
//! and don't leak into the parent `ask.ask()` transaction.

use super::render;
use super::{Tool, ToolOutput};
use crate::infra::errors::{AskError, Result};
use crate::providers::ToolSpec;
use pgrx::prelude::*;
use serde_json::Value;

/// Default cap on rows returned to the model from a user-defined tool.
/// Operators can't override this per-tool yet (planned: H6 in the
/// review). Match `sql_query`'s default so the model sees the same
/// budget regardless of which path it went through.
const USER_TOOL_ROW_CAP: usize = 100;

pub struct UserDefinedTool {
    pub name: String,
    pub body: String,
    pub spec: ToolSpec,
    /// Mirrors `SqlQueryTool::readonly`: when true, the subtxn that
    /// runs the body issues `SET LOCAL transaction_read_only = on`
    /// so even an operator-blessed body cannot write while the global
    /// readonly switch is engaged.
    pub readonly: bool,
    /// Per-call `SET LOCAL statement_timeout` (milliseconds). 0
    /// disables. Same source GUC as the built-in tools so operators
    /// configure them in one place.
    pub statement_timeout_ms: u64,
}

impl Tool for UserDefinedTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn invoke(&self, args: &Value) -> Result<ToolOutput> {
        let plan = build_plan(&self.body, args).map_err(|e| AskError::Tool {
            name: self.name.clone(),
            message: e,
        })?;

        match run_planned(&plan, self.readonly, self.statement_timeout_ms) {
            Ok(text) => Ok(ToolOutput {
                text,
                is_error: false,
            }),
            Err(e) => Ok(ToolOutput {
                text: format!("tool `{}` failed: {e}", self.name),
                is_error: true,
            }),
        }
    }
}

/// One bound argument resolved from the tool's input jsonb.
///
/// We keep the JSON `Value` rather than pre-converting to a pgrx Datum:
/// the conversion happens later under the `Spi::connect` callback so
/// every datum we hand SPI is freshly built in the right memory context.
#[derive(Debug, PartialEq)]
enum Bound {
    Text(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    /// Composite values (arrays / objects) are serialised as JSON text and
    /// passed in with an explicit `::jsonb` cast in the rewritten body so
    /// the planner doesn't have to guess the type.
    Jsonb(String),
}

#[derive(Debug, PartialEq)]
struct Plan {
    /// Body with each `{{key}}` replaced by `$N` (or `($N::jsonb)` for
    /// composite values). Placeholders start at `$2` because `$1` is the
    /// row cap added by `render::wrap_with_cap`.
    rewritten_body: String,
    /// Bind values in `$2..` order.
    bindings: Vec<Bound>,
}

/// Walk `template` for `{{key}}` markers, look each one up in `args`,
/// and assemble a `Plan` of the rewritten SQL + bind values.
///
/// Same `{{key}}` repeated in the body reuses the same `$N` placeholder
/// (and binds the value once), which is both faster and matches what
/// hand-written parameterised SQL would do.
fn build_plan(template: &str, args: &Value) -> std::result::Result<Plan, String> {
    let obj = match args {
        Value::Object(map) => map,
        // Empty / non-object args: still scan for unresolved placeholders
        // so we surface the "missing argument" error instead of silently
        // running with a literal `{{key}}` in the SQL.
        _ => {
            if template.contains("{{") {
                return Err("user-defined tool called with non-object arguments".into());
            }
            return Ok(Plan {
                rewritten_body: template.to_string(),
                bindings: Vec::new(),
            });
        }
    };

    // First pass: find every unique placeholder name in the order it
    // first appears. Order matters because the user's body might mix
    // `{{a}}` and `{{b}}` arbitrarily and we want $2..$N to be stable.
    let mut placeholder_order: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    while let Some(open) = template[cursor..].find("{{") {
        let abs_open = cursor + open;
        let after_open = abs_open + 2;
        let close = template[after_open..]
            .find("}}")
            .ok_or_else(|| "unterminated `{{` placeholder in tool body".to_string())?;
        let key = template[after_open..after_open + close].trim().to_string();
        if key.is_empty() {
            return Err("empty `{{}}` placeholder in tool body".into());
        }
        if !placeholder_order.contains(&key) {
            placeholder_order.push(key);
        }
        cursor = after_open + close + 2;
    }

    // Bind values, in $2..$N order.
    let mut bindings: Vec<Bound> = Vec::with_capacity(placeholder_order.len());
    for key in &placeholder_order {
        let val = obj
            .get(key)
            .ok_or_else(|| format!("missing argument `{key}` for tool"))?;
        bindings.push(json_to_bound(val));
    }

    // Second pass: rewrite the template with placeholders. We do this in
    // one linear scan rather than `String::replace` so two unrelated
    // placeholders with overlapping substrings (`{{a}}` vs `{{ab}}`)
    // can't interfere.
    let mut rewritten = String::with_capacity(template.len());
    let mut cursor = 0usize;
    while let Some(open) = template[cursor..].find("{{") {
        let abs_open = cursor + open;
        rewritten.push_str(&template[cursor..abs_open]);
        let after_open = abs_open + 2;
        let close = template[after_open..]
            .find("}}")
            .expect("validated in first pass");
        let key = template[after_open..after_open + close].trim();
        let idx = placeholder_order
            .iter()
            .position(|k| k == key)
            .expect("inserted in first pass");
        // $2 onwards \u2014 $1 is reserved for the row cap.
        let pg_idx = idx + 2;
        let bind = &bindings[idx];
        // jsonb composite values need an explicit cast so the planner
        // can resolve operator overloading; scalars infer fine.
        match bind {
            Bound::Jsonb(_) => rewritten.push_str(&format!("(${pg_idx}::jsonb)")),
            _ => rewritten.push_str(&format!("${pg_idx}")),
        }
        cursor = after_open + close + 2;
    }
    rewritten.push_str(&template[cursor..]);

    Ok(Plan {
        rewritten_body: rewritten,
        bindings,
    })
}

fn json_to_bound(v: &Value) -> Bound {
    match v {
        Value::Null => Bound::Null,
        Value::Bool(b) => Bound::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Bound::Int(i)
            } else if let Some(f) = n.as_f64() {
                Bound::Float(f)
            } else {
                // serde_json arbitrary-precision path \u2014 stringify
                // so the user can cast it server-side.
                Bound::Text(n.to_string())
            }
        }
        Value::String(s) => Bound::Text(s.clone()),
        Value::Array(_) | Value::Object(_) => Bound::Jsonb(v.to_string()),
    }
}

fn run_planned(
    plan: &Plan,
    readonly: bool,
    statement_timeout_ms: u64,
) -> std::result::Result<String, String> {
    let wrapped = render::wrap_with_cap(&plan.rewritten_body);
    let cap_plus_one = (USER_TOOL_ROW_CAP.saturating_add(1)) as i64;

    // HP1: own everything the closure needs so it satisfies the
    // `FnOnce + UnwindSafe` bound that `run_in_subtransaction`
    // imposes. The body string and bindings are cheap to clone
    // — the model's argument values are bounded by the prompt size.
    let wrapped_owned = wrapped;
    let bindings_owned: Vec<Bound> = plan.bindings.iter().map(clone_bound).collect();

    let subtxn_result =
        crate::infra::subtxn::run_in_subtransaction(Some("pg_ask_user_tool"), move || {
            // Apply the per-call GUCs from INSIDE the subtxn so they
            // auto-revert when the subtxn releases. Without this the
            // `SET LOCAL transaction_read_only = on` would persist
            // for the rest of the outer ask.ask() / ask.chat()
            // transaction and every later INSERT (telemetry, session
            // append, next audit row …) would fail with 25006.
            apply_per_call_gucs(readonly, statement_timeout_ms)?;

            Spi::connect(|client| -> Result<String> {
                let mut args: Vec<pgrx::datum::DatumWithOid> =
                    Vec::with_capacity(bindings_owned.len() + 1);
                args.push(cap_plus_one.into());
                for b in &bindings_owned {
                    args.push(match b {
                        Bound::Text(s) => s.as_str().into(),
                        Bound::Int(i) => (*i).into(),
                        Bound::Float(f) => (*f).into(),
                        Bound::Bool(b) => (*b).into(),
                        // NULL routing through pgrx: an
                        // Option::<&str>::None becomes a NULL text
                        // datum, which the planner can coerce to
                        // whatever the surrounding context expects.
                        Bound::Null => Option::<&str>::None.into(),
                        Bound::Jsonb(s) => s.as_str().into(),
                    });
                }

                let tuptable = client
                    .select(&wrapped_owned, Some(cap_plus_one), &args)
                    .map_err(|e| AskError::Sql(e.to_string()))?;

                let (json_rows, truncated) =
                    render::parse_json_rows(tuptable, USER_TOOL_ROW_CAP).map_err(AskError::Sql)?;
                // No sensitive-column filtering: user-defined tool
                // bodies are operator-authored, so the operator
                // already controls what columns leak. Per-tool
                // sensitivity could be added later via the spec JSON.
                let (text, _) = render::format_table(&json_rows, &[], truncated, USER_TOOL_ROW_CAP);
                Ok(text)
            })
        });

    subtxn_result.map_err(|e| e.to_string())
}

/// Mirror of `sql_query::apply_per_call_gucs` — kept private here so
/// the user-defined path can evolve its own policy without coupling to
/// the built-in tools. Both share the same `SET LOCAL` scoping trick:
/// the GUCs are scoped to the subtxn that called this function and
/// auto-revert on release.
fn apply_per_call_gucs(readonly: bool, statement_timeout_ms: u64) -> Result<()> {
    let timeout_sql = format!("SET LOCAL statement_timeout = {statement_timeout_ms}");
    Spi::connect_mut(|client| -> Result<()> {
        client
            .update(timeout_sql.as_str(), None, &[])
            .map_err(|e| AskError::Sql(e.to_string()))?;
        if readonly {
            client
                .update("SET LOCAL transaction_read_only = on", None, &[])
                .map_err(|e| AskError::Sql(e.to_string()))?;
        }
        Ok(())
    })
}

/// Bound is private + non-Clone so the public test surface stays
/// minimal; deep-clone manually here for the subtxn closure's `move`.
fn clone_bound(b: &Bound) -> Bound {
    match b {
        Bound::Text(s) => Bound::Text(s.clone()),
        Bound::Int(i) => Bound::Int(*i),
        Bound::Float(f) => Bound::Float(*f),
        Bound::Bool(b) => Bound::Bool(*b),
        Bound::Null => Bound::Null,
        Bound::Jsonb(s) => Bound::Jsonb(s.clone()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn injection_via_string_arg_is_neutralised() {
        // Pre-v0.5.2 this template would have produced:
        //   SELECT * FROM users WHERE age >= 0; DROP TABLE _config; --
        // i.e. two statements. Post-fix it produces a single parameterised
        // statement; the entire malicious string is bound as one text
        // value to $2 and never parsed as SQL.
        let body = "SELECT * FROM users WHERE age >= {{min_age}}";
        let args = json!({ "min_age": "0; DROP TABLE _config; --" });
        let plan = build_plan(body, &args).unwrap();
        assert_eq!(plan.rewritten_body, "SELECT * FROM users WHERE age >= $2");
        assert_eq!(
            plan.bindings,
            vec![Bound::Text("0; DROP TABLE _config; --".into())]
        );
    }

    #[test]
    fn repeated_placeholder_reuses_same_param() {
        let body = "SELECT {{n}} AS a, {{n}} + 1 AS b WHERE x > {{n}}";
        let args = json!({ "n": 42 });
        let plan = build_plan(body, &args).unwrap();
        assert_eq!(plan.bindings, vec![Bound::Int(42)]);
        assert_eq!(
            plan.rewritten_body,
            "SELECT $2 AS a, $2 + 1 AS b WHERE x > $2"
        );
    }

    #[test]
    fn multiple_placeholders_get_distinct_params() {
        // Argument order in the args object must not affect param numbering;
        // numbering follows first-appearance order in the body.
        let body = "SELECT * WHERE a = {{first}} AND b = {{second}}";
        let args = json!({ "second": "B", "first": "A" });
        let plan = build_plan(body, &args).unwrap();
        assert_eq!(plan.rewritten_body, "SELECT * WHERE a = $2 AND b = $3");
        assert_eq!(
            plan.bindings,
            vec![Bound::Text("A".into()), Bound::Text("B".into())]
        );
    }

    #[test]
    fn json_value_types_map_to_expected_bound_variants() {
        let body = "SELECT {{s}}, {{i}}, {{f}}, {{b}}, {{nil}}, {{arr}}, {{obj}}";
        let args = json!({
            "s":   "hello",
            "i":   42,
            "f":   3.14,
            "b":   true,
            "nil": null,
            "arr": [1, 2, 3],
            "obj": {"k": "v"},
        });
        let plan = build_plan(body, &args).unwrap();
        // First appearance order: s, i, f, b, nil, arr, obj
        let kinds: Vec<&str> = plan
            .bindings
            .iter()
            .map(|b| match b {
                Bound::Text(_) => "text",
                Bound::Int(_) => "int",
                Bound::Float(_) => "float",
                Bound::Bool(_) => "bool",
                Bound::Null => "null",
                Bound::Jsonb(_) => "jsonb",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["text", "int", "float", "bool", "null", "jsonb", "jsonb"]
        );
        // Composite values get the explicit ::jsonb cast in the rewrite.
        assert!(plan.rewritten_body.contains("($7::jsonb)"));
        assert!(plan.rewritten_body.contains("($8::jsonb)"));
    }

    #[test]
    fn missing_argument_is_reported_not_silently_left_in() {
        let body = "SELECT {{missing}}";
        let err = build_plan(body, &json!({})).unwrap_err();
        assert!(err.contains("missing argument"), "got: {err}");
    }

    #[test]
    fn unterminated_placeholder_is_an_error() {
        let body = "SELECT {{never_closed";
        let err = build_plan(body, &json!({})).unwrap_err();
        assert!(err.contains("unterminated"), "got: {err}");
    }

    #[test]
    fn whitespace_inside_placeholder_is_ignored() {
        let body = "SELECT {{  spaced  }}";
        let plan = build_plan(body, &json!({"spaced": 1})).unwrap();
        assert_eq!(plan.rewritten_body, "SELECT $2");
        assert_eq!(plan.bindings, vec![Bound::Int(1)]);
    }

    #[test]
    fn overlapping_placeholder_names_dont_collide() {
        // Regression-style: with naive `String::replace` substituting `{{a}}`
        // first would also damage `{{ab}}`. Our single-pass walker is
        // immune by construction; this test pins the behaviour.
        let body = "{{a}} + {{ab}} + {{a}}";
        let plan = build_plan(body, &json!({"a": 1, "ab": 2})).unwrap();
        assert_eq!(plan.rewritten_body, "$2 + $3 + $2");
        assert_eq!(plan.bindings, vec![Bound::Int(1), Bound::Int(2)]);
    }

    #[test]
    fn no_placeholders_passes_through() {
        let body = "SELECT 1";
        let plan = build_plan(body, &json!({})).unwrap();
        assert_eq!(plan.rewritten_body, "SELECT 1");
        assert!(plan.bindings.is_empty());
    }
}
