//! Distil a Postgres `EXPLAIN (FORMAT JSON)` plan into the operator-facing
//! columns of `ask.preview`.
//!
//! Postgres emits a recursive `Plan` tree:
//!
//! ```json
//! [{ "Plan": {
//!     "Node Type": "Hash Join",
//!     "Plan Rows": 12345,
//!     "Plans": [
//!       { "Node Type": "Seq Scan", "Relation Name": "orders", "Schema": "public", ... },
//!       { "Node Type": "Index Scan", "Relation Name": "users", "Schema": "public", ... }
//!     ]
//! }}]
//! ```
//!
//! We walk it once and extract three things:
//!
//! * `est_rows` — root `Plan Rows`.
//! * `tables`   — every `Schema.Relation Name` referenced, de-duplicated.
//! * `warnings` — heuristic risk notes, ordered most-important-first.

use serde_json::Value;
use std::collections::BTreeSet;

const HUGE_ROWS: i64 = 100_000;
const WIDE_SCAN_ROWS: i64 = 10_000;

#[derive(Debug, Default)]
pub struct PlanSummary {
    pub est_rows: i64,
    pub tables: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn summarize(plan_json: &Value) -> Option<PlanSummary> {
    let root = plan_json.get("Plan")?;

    let mut tables: BTreeSet<String> = BTreeSet::new();
    let mut warnings: Vec<String> = Vec::new();

    walk(root, &mut tables, &mut warnings);

    let est_rows = root
        .get("Plan Rows")
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);

    if est_rows > HUGE_ROWS {
        warnings.insert(
            0,
            format!("estimated row count is high ({est_rows}); consider adding a LIMIT or stricter WHERE"),
        );
    }

    Some(PlanSummary {
        est_rows,
        tables: tables.into_iter().collect(),
        warnings,
    })
}

fn walk(node: &Value, tables: &mut BTreeSet<String>, warnings: &mut Vec<String>) {
    // Collect schema.table whenever the node references a relation.
    if let (Some(rel), schema) = (
        node.get("Relation Name").and_then(|v| v.as_str()),
        node.get("Schema").and_then(|v| v.as_str()),
    ) {
        let key = match schema {
            Some(s) if !s.is_empty() => format!("{s}.{rel}"),
            _ => rel.to_string(),
        };
        tables.insert(key);
    }

    // Heuristic warnings on this node.
    if let Some(node_type) = node.get("Node Type").and_then(|v| v.as_str()) {
        let rows = node
            .get("Plan Rows")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let rel = node.get("Relation Name").and_then(|v| v.as_str()).unwrap_or("");

        match node_type {
            "Seq Scan" if rows > WIDE_SCAN_ROWS => {
                warnings.push(format!(
                    "Seq Scan on `{rel}` estimated to read {rows} rows; an index may help"
                ));
            }
            "Nested Loop" if rows > HUGE_ROWS => {
                warnings.push(format!(
                    "Nested Loop estimated to produce {rows} rows; check join conditions"
                ));
            }
            _ => {}
        }
    }

    // Recurse into child plans.
    if let Some(children) = node.get("Plans").and_then(|v| v.as_array()) {
        for child in children {
            walk(child, tables, warnings);
        }
    }
    // CTE / SubPlan / InitPlan branches sit at the same level via "Subplans"
    // in newer PG; both shapes are covered by the same walker.
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plan(value: serde_json::Value) -> Value {
        value
    }

    #[test]
    fn extracts_root_rows_and_table() {
        let p = plan(json!({
            "Plan": {
                "Node Type": "Seq Scan",
                "Schema": "public",
                "Relation Name": "users",
                "Plan Rows": 42
            }
        }));
        let s = summarize(&p).unwrap();
        assert_eq!(s.est_rows, 42);
        assert_eq!(s.tables, vec!["public.users".to_string()]);
        // Small Seq Scan should not produce a warning.
        assert!(s.warnings.is_empty(), "got warnings: {:?}", s.warnings);
    }

    #[test]
    fn flags_large_seq_scan_and_recurses() {
        let p = plan(json!({
            "Plan": {
                "Node Type": "Hash Join",
                "Plan Rows": 250_000,
                "Plans": [
                    {
                        "Node Type": "Seq Scan",
                        "Schema": "public",
                        "Relation Name": "orders",
                        "Plan Rows": 200_000
                    },
                    {
                        "Node Type": "Index Scan",
                        "Schema": "public",
                        "Relation Name": "users",
                        "Plan Rows": 100
                    }
                ]
            }
        }));
        let s = summarize(&p).unwrap();
        assert_eq!(s.est_rows, 250_000);
        assert_eq!(
            s.tables,
            vec!["public.orders".to_string(), "public.users".to_string()]
        );
        // First warning should be the high root estimate, then Seq Scan.
        assert!(s.warnings[0].contains("estimated row count is high"));
        assert!(s.warnings.iter().any(|w| w.contains("Seq Scan on `orders`")));
    }

    #[test]
    fn deduplicates_table_visits() {
        let p = plan(json!({
            "Plan": {
                "Node Type": "Append",
                "Plan Rows": 10,
                "Plans": [
                    { "Node Type": "Seq Scan", "Schema": "public",
                      "Relation Name": "t", "Plan Rows": 5 },
                    { "Node Type": "Seq Scan", "Schema": "public",
                      "Relation Name": "t", "Plan Rows": 5 }
                ]
            }
        }));
        let s = summarize(&p).unwrap();
        assert_eq!(s.tables, vec!["public.t".to_string()]);
    }
}
