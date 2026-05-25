//! SQL safety guard.
//!
//! Validates strings the model wants to execute, before they reach SPI.
//! Belt-and-braces over `transaction_read_only` — the guard is one layer,
//! the readonly transaction is another, RLS/GRANTs are the primary defence.
//!
//! v0.5 upgrade: statement-type classification now uses a real SQL parser
//! (`sqlparser` with PostgreSQL dialect). The token-based lexer is kept
//! as a fallback when the parser chokes on non-standard Postgres syntax,
//! and for the function-denylist check (which needs to distinguish
//! function names from string literals and identifiers).

mod lexer;
mod rules;

use crate::infra::errors::{AskError, Result};

/// What the guard is willing to let through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardMode {
    /// `pg_ask.readonly = true` (default): only single-statement
    /// SELECT / WITH-SELECT / EXPLAIN / TABLE, no banned functions.
    Readonly,
    /// `pg_ask.readonly = false`: still single-statement, still no banned
    /// functions, but writes are permitted. The readonly transaction wrap
    /// is dropped at the SPI call site.
    Writable,
}

/// A statement that has passed every rule. The newtype prevents
/// accidentally executing an unvalidated string further down.
#[derive(Debug)]
pub struct ValidatedSql<'a> {
    sql: &'a str,
}

impl<'a> ValidatedSql<'a> {
    pub fn as_str(&self) -> &'a str {
        self.sql
    }
}

/// Run every rule. On failure, returns a structured error whose message is
/// safe to surface to the model so it can self-correct.
pub fn validate(sql: &str, mode: GuardMode) -> Result<ValidatedSql<'_>> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err(AskError::GuardRejected("empty statement".into()));
    }

    // v0.5.2: parser is *authoritative* whenever it succeeds. A previous
    // version fell back to the lexer whenever AST checks rejected, which
    // silently allowed any verb in `WRITE_VERBS` (e.g. `DROP TABLE`) to slip
    // through in writable mode — the lexer only ever knows the first verb.
    // The lexer fallback is for queries the parser cannot parse at all
    // (non-standard Postgres syntax like `EXPLAIN (FORMAT XML, ...)`,
    // operator-defined statements, etc.).
    match parse_ast(trimmed) {
        Ok(stmts) => {
            ast_checks(&stmts, mode)?;
        }
        Err(_) => {
            let tokens = lexer::tokenize(trimmed);
            rules::single_statement(&tokens)?;
            rules::starts_with_allowed_verb(&tokens, mode)?;
        }
    }

    // Function denylist is always checked via the token stream so we
    // correctly ignore banned names inside string literals.
    let tokens = lexer::tokenize(trimmed);
    rules::no_banned_functions(&tokens)?;

    Ok(ValidatedSql { sql: trimmed })
}

// ---------- v0.5 real parser ----------

fn parse_ast(sql: &str) -> std::result::Result<Vec<sqlparser::ast::Statement>, sqlparser::parser::ParserError> {
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    let dialect = PostgreSqlDialect {};
    Parser::parse_sql(&dialect, sql)
}

fn ast_checks(stmts: &[sqlparser::ast::Statement], mode: GuardMode) -> Result<()> {
    if stmts.len() != 1 {
        return Err(AskError::GuardRejected(
            "only one statement allowed".into(),
        ));
    }

    // C8 (v0.5.2 review): walk every relation and function call in the
    // statement and reject anything that would let a model probe the
    // extension's secrets via the GUC layer:
    //
    //   SELECT current_setting('pg_ask.api_key')
    //   SELECT setting FROM pg_settings WHERE name = 'pg_ask.api_key'
    //   ALTER SYSTEM SET pg_ask.api_key = '...'   (caught here AND by DDL guard)
    //
    // The token-level deny-list in `rules::no_banned_functions` catches
    // the bare-identifier case but misses quoted forms
    //   "current_setting"('pg_ask.api_key')
    //   "pg_catalog"."current_setting"('pg_ask.api_key')
    // because the lexer doesn't strip the quotes before comparison.
    // Walking the AST normalises identifiers for us.
    secrets_visitor::check(&stmts[0])?;

    use sqlparser::ast::Statement;
    match &stmts[0] {
        // Read-only shapes
        Statement::Query(_) => Ok(()),
        Statement::Explain { .. } => Ok(()),
        Statement::Copy { .. } => Err(AskError::GuardRejected(
            "COPY is not allowed".into(),
        )),
        // Write shapes — permitted only in writable mode
        Statement::Insert(_)
        | Statement::Update { .. }
        | Statement::Delete(_) => {
            if mode == GuardMode::Readonly {
                Err(AskError::GuardRejected(
                    "write statements are not allowed in readonly mode".into(),
                ))
            } else {
                Ok(())
            }
        }
        // DDL — always rejected (even in writable mode the operator should
        // run DDL themselves, not via the model).
        Statement::CreateTable(_)
        | Statement::CreateView(_)
        | Statement::CreateIndex(_)
        | Statement::Drop { .. }
        | Statement::AlterTable(_)
        | Statement::Truncate { .. }
        | Statement::CreateSchema { .. }
        | Statement::CreateSequence { .. }
        | Statement::CreateExtension { .. } => Err(AskError::GuardRejected(
            "DDL statements are not allowed".into(),
        )),
        // Everything else is blocked by default.
        other => {
            let kind = format!("{other:?}");
            let short = kind.split('(').next().unwrap_or(&kind);
            Err(AskError::GuardRejected(format!(
                "statement type `{short}` is not allowed"
            )))
        }
    }
}

/// AST walker that rejects any reference to the extension's own GUC
/// namespace (`pg_ask.*`) via `current_setting`, `set_config`, or the
/// `pg_settings` / `pg_file_settings` / `pg_db_role_setting` catalog
/// views. See C8 in the v0.5.2 review.
///
/// We don't need an FQN matcher here — sqlparser's `ObjectName` already
/// preserves the schema-qualified form, and identifier comparison is
/// case-insensitive after lowercasing the parts. Quoted identifiers
/// (`"current_setting"`) survive parsing as ordinary `Ident`s, so the
/// quote bypass that fooled the token-level checker doesn't apply.
mod secrets_visitor {
    use crate::infra::errors::{AskError, Result};
    use sqlparser::ast::{Expr, Function, FunctionArg, FunctionArgExpr, FunctionArgumentList,
                          FunctionArguments, ObjectName, Statement, Value, ValueWithSpan,
                          Visit, Visitor};
    use std::ops::ControlFlow;

    /// Catalog views that expose GUC values. Reading any of these from
    /// inside a model-issued query is never legitimate — the model
    /// doesn't need to introspect server configuration to answer
    /// questions about user data, and these views are the second-line
    /// way (after current_setting) to fish out our api_key.
    const BANNED_RELATIONS: &[&str] = &[
        "pg_settings",
        "pg_file_settings",
        "pg_db_role_setting",
        "pg_hba_file_rules",
        "pg_ident_file_mappings",
        "pg_shadow",    // password hashes
        "pg_authid",    // ditto
        "pg_user_mapping",
    ];

    /// Functions that take a GUC name and either read or write it.
    /// `set_config` is already in the lexer-level deny list, but having
    /// it here too means the quoted-identifier bypass
    /// (`"set_config"(...)`) doesn't leak past the AST checker.
    const GUC_FUNCTIONS: &[&str] = &["current_setting", "set_config"];

    pub fn check(stmt: &Statement) -> Result<()> {
        let mut v = Walker::default();
        match stmt.visit(&mut v) {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(msg) => Err(AskError::GuardRejected(msg)),
        }
    }

    #[derive(Default)]
    struct Walker;

    impl Visitor for Walker {
        type Break = String;

        fn pre_visit_relation(&mut self, name: &ObjectName) -> ControlFlow<Self::Break> {
            // Last identifier is the relation; preceding parts are
            // schema / database qualifiers. We reject by *unqualified*
            // name so `pg_catalog.pg_settings` is caught alongside the
            // bare form.
            let last = match name.0.last() {
                Some(part) => part.to_string().to_ascii_lowercase(),
                None => return ControlFlow::Continue(()),
            };
            // Identifier::to_string() preserves quotes around quoted
            // identifiers (e.g. `"pg_settings"` → `"pg_settings"`).
            // Strip them so the comparison normalises.
            let normalized = last.trim_matches('"');
            if BANNED_RELATIONS.iter().any(|b| b.eq_ignore_ascii_case(normalized)) {
                return ControlFlow::Break(format!(
                    "access to system catalog `{normalized}` is not allowed (would expose GUCs / secrets)"
                ));
            }
            ControlFlow::Continue(())
        }

        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
            if let Expr::Function(func) = expr {
                if let Some(msg) = check_function_call(func) {
                    return ControlFlow::Break(msg);
                }
            }
            ControlFlow::Continue(())
        }
    }

    fn check_function_call(func: &Function) -> Option<String> {
        // ObjectName for the function: e.g. ["pg_catalog", "current_setting"]
        // or just ["current_setting"]. Match on the unqualified tail and
        // ignore the schema — same reasoning as relations above.
        let name = func.name.0.last()?.to_string().to_ascii_lowercase();
        let normalized = name.trim_matches('"');
        if !GUC_FUNCTIONS.iter().any(|f| f.eq_ignore_ascii_case(normalized)) {
            return None;
        }

        // Pull the first argument as a string literal if we can. We're
        // conservative: if we can't see a literal, we reject. A model
        // dynamically composing the GUC name (e.g.
        // `current_setting('pg_ask.' || 'api_key')`) is exactly what
        // we're trying to prevent, so falling closed is correct.
        let first_arg = first_arg_string(&func.args);
        match first_arg {
            Some(s) if !is_protected_guc(&s) => None,
            Some(s) => Some(format!(
                "`{normalized}(\u{2026})` on GUC `{s}` is not allowed (extension-internal namespace)"
            )),
            None => Some(format!(
                "`{normalized}(\u{2026})` requires a string literal first argument when used by the model"
            )),
        }
    }

    /// Extension-internal GUC namespaces. We reject reads/writes of
    /// anything inside these prefixes so secrets stored as GUCs (e.g.
    /// `pg_ask.api_key`, `pg_ask.embedding_api_key`) can't be exfiltrated.
    ///
    /// We also block the matching reads of common extension-secret
    /// namespaces (`vault.*` from supabase-vault, `app.secrets.*` as a
    /// convention) on the principle that the model has no business
    /// poking at extension config no matter whose extension it is.
    fn is_protected_guc(name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        const PROTECTED_PREFIXES: &[&str] = &[
            "pg_ask.",
            "vault.",
            "app.secrets.",
        ];
        PROTECTED_PREFIXES.iter().any(|p| lower.starts_with(p))
    }

    fn first_arg_string(args: &FunctionArguments) -> Option<String> {
        let list: &FunctionArgumentList = match args {
            FunctionArguments::List(list) => list,
            _ => return None,
        };
        let first = list.args.first()?;
        let expr = match first {
            FunctionArg::Named { arg, .. } | FunctionArg::ExprNamed { arg, .. } => arg,
            FunctionArg::Unnamed(e) => e,
        };
        match expr {
            FunctionArgExpr::Expr(Expr::Value(ValueWithSpan { value, .. })) => match value {
                Value::SingleQuotedString(s)
                | Value::DoubleQuotedString(s)
                | Value::EscapedStringLiteral(s)
                | Value::NationalStringLiteral(s)
                | Value::DollarQuotedString(sqlparser::ast::DollarQuotedString { value: s, .. }) => {
                    Some(s.clone())
                }
                _ => None,
            },
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(sql: &str) -> bool {
        validate(sql, GuardMode::Readonly).is_ok()
    }
    fn rejected(sql: &str) -> bool {
        validate(sql, GuardMode::Readonly).is_err()
    }

    #[test]
    fn accepts_basic_select() {
        assert!(ok("SELECT 1"));
        assert!(ok("  select * from t  "));
        assert!(ok("WITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(ok("EXPLAIN SELECT 1"));
        assert!(ok("TABLE pg_class"));
    }

    #[test]
    fn trailing_semicolon_ok() {
        assert!(ok("SELECT 1;"));
        assert!(ok("SELECT 1;   "));
    }

    #[test]
    fn rejects_current_setting_on_extension_guc() {
        // C8 (v0.5.2 review): model probing extension secrets via the
        // GUC layer is blocked at AST level.
        assert!(rejected("SELECT current_setting('pg_ask.api_key')"));
        assert!(rejected("SELECT current_setting('pg_ask.embedding_api_key')"));
        // Quoted-identifier bypass that the lexer-level deny list
        // misses: AST walker normalises the identifier.
        assert!(rejected("SELECT \"current_setting\"('pg_ask.api_key')"));
        // Schema-qualified call form.
        assert!(rejected("SELECT pg_catalog.current_setting('pg_ask.api_key')"));
        // Dynamic name composition falls closed (we can't see the literal).
        assert!(rejected("SELECT current_setting('pg_ask.' || 'api_key')"));
        // set_config writes to the same namespace — blocked too.
        assert!(rejected("SELECT set_config('pg_ask.api_key', 'sk-evil', false)"));
    }

    #[test]
    fn allows_current_setting_on_unrelated_gucs() {
        // Reading non-extension GUCs (work_mem, search_path, app.*) is fine.
        assert!(ok("SELECT current_setting('work_mem')"));
        assert!(ok("SELECT current_setting('search_path')"));
        assert!(ok("SELECT current_setting('app.user_id')"));
    }

    #[test]
    fn rejects_pg_settings_and_friends() {
        assert!(rejected("SELECT setting FROM pg_settings WHERE name = 'pg_ask.api_key'"));
        assert!(rejected("SELECT * FROM pg_catalog.pg_settings"));
        assert!(rejected("SELECT * FROM pg_file_settings"));
        assert!(rejected("SELECT * FROM pg_db_role_setting"));
        assert!(rejected("SELECT * FROM pg_shadow"));
        assert!(rejected("SELECT * FROM pg_authid"));
        // Other catalogs remain accessible — model can still describe schema.
        assert!(ok("SELECT * FROM pg_class LIMIT 1"));
        assert!(ok("SELECT * FROM pg_namespace LIMIT 1"));
    }

    fn writable_ok(sql: &str) -> bool {
        validate(sql, GuardMode::Writable).is_ok()
    }
    fn writable_rejected(sql: &str) -> bool {
        validate(sql, GuardMode::Writable).is_err()
    }

    #[test]
    fn writable_mode_allows_dml_only_not_ddl() {
        // Regression: prior to v0.5.2 the parser-vs-lexer fallback let any
        // verb in WRITE_VERBS through (drop, alter, create, truncate, grant).
        // The AST is now authoritative when it parses successfully.
        assert!(writable_ok("INSERT INTO t VALUES (1)"));
        assert!(writable_ok("UPDATE t SET a = 1"));
        assert!(writable_ok("DELETE FROM t WHERE a = 1"));

        // DDL must be rejected regardless of mode.
        assert!(writable_rejected("DROP TABLE t"));
        assert!(writable_rejected("ALTER TABLE t ADD COLUMN c int"));
        assert!(writable_rejected("CREATE TABLE t (a int)"));
        assert!(writable_rejected("TRUNCATE t"));
        assert!(writable_rejected("CREATE INDEX idx ON t (a)"));
        assert!(writable_rejected("CREATE SCHEMA s"));
        // GRANT / REVOKE not in AST cover above — they tokenize to a verb in
        // WRITE_VERBS, so the lexer fallback would have accepted them. With
        // parser-authoritative validation they hit the catch-all reject arm.
        assert!(writable_rejected("GRANT SELECT ON t TO public"));
        assert!(writable_rejected("REVOKE ALL ON t FROM public"));
    }

    #[test]
    fn rejects_writes_in_readonly() {
        assert!(rejected("INSERT INTO t VALUES (1)"));
        assert!(rejected("UPDATE t SET a = 1"));
        assert!(rejected("DELETE FROM t"));
        assert!(rejected("DROP TABLE t"));
        assert!(rejected("ALTER TABLE t ADD COLUMN c int"));
        assert!(rejected("TRUNCATE t"));
        assert!(rejected("CREATE TABLE t (a int)"));
    }

    #[test]
    fn rejects_multi_statement() {
        assert!(rejected("SELECT 1; SELECT 2"));
        assert!(rejected("SELECT 1; DROP TABLE t"));
    }

    #[test]
    fn rejects_banned_functions() {
        assert!(rejected("SELECT pg_sleep(60)"));
        assert!(rejected("SELECT pg_read_file('x')"));
        assert!(rejected("SELECT * FROM dblink('x','y')"));
        assert!(rejected("COPY t TO PROGRAM 'rm -rf'"));
        assert!(rejected("SELECT pg_terminate_backend(1)"));
        assert!(rejected("SELECT set_config('x', 'y', false)"));
    }

    #[test]
    fn ignores_banned_words_in_strings_and_identifiers() {
        // Function name *in a string literal* is fine — it is data, not code.
        assert!(ok("SELECT 'pg_sleep is bad'"));
        // Identifier containing a banned name should still be fine if not called.
        assert!(ok("SELECT my_pg_sleep_marker FROM t"));
    }

    #[test]
    fn comments_do_not_smuggle_writes() {
        // Real first verb is SELECT; the comment is just noise.
        assert!(ok("-- DROP TABLE t\nSELECT 1"));
        assert!(ok("/* DROP TABLE t */ SELECT 1"));
        // But a real second statement after a comment must still be rejected.
        assert!(rejected("SELECT 1; /* comment */ DROP TABLE t"));
    }
}
