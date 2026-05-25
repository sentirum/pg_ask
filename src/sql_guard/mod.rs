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

    // v0.5: try real parser first for statement-type classification.
    let parsed_ok = if let Ok(stmts) = parse_ast(trimmed) {
        ast_checks(&stmts, mode).is_ok()
    } else {
        false
    };

    let tokens = lexer::tokenize(trimmed);

    if !parsed_ok {
        // Parser couldn't classify the statement — fall back to the lexer.
        rules::single_statement(&tokens)?;
        rules::starts_with_allowed_verb(&tokens, mode)?;
    }

    // Function denylist is always checked via the token stream so we
    // correctly ignore banned names inside string literals.
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
