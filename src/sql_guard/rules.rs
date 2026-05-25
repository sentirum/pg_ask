//! Guard rules — pure functions over the token stream from `lexer.rs`.
//!
//! Each rule has one job. Adding a new rule means one function + one call
//! site in `mod::validate`. Test coverage lives in `mod::tests`.

use super::lexer::Token;
use super::GuardMode;
use crate::infra::errors::{AskError, Result};

/// Verbs we will let pass when `mode = Readonly`. We are lenient about
/// `EXPLAIN` because `EXPLAIN (ANALYZE) DROP …` would still need a
/// non-read-only transaction to take effect; the readonly txn catches it.
const READONLY_VERBS: &[&str] = &["select", "with", "table", "explain", "values", "show"];

/// Verbs we explicitly call out as writes when `mode = Writable`. Anything
/// else still has to start with one of these or the readonly list.
const WRITE_VERBS: &[&str] = &[
    "insert", "update", "delete", "merge", "truncate", "create", "alter", "drop", "grant",
    "revoke", "comment", "refresh", "vacuum", "analyze", "reindex", "cluster", "lock",
];

/// Function names we never want the model to call, regardless of mode.
/// Matched case-insensitively against bareword identifiers followed by `(`.
const BANNED_FUNCTIONS: &[&str] = &[
    // Time-waste / DoS
    "pg_sleep",
    "pg_sleep_for",
    "pg_sleep_until",
    // Filesystem reach
    "pg_read_file",
    "pg_read_binary_file",
    "pg_read_server_files",
    "pg_ls_dir",
    "pg_stat_file",
    "lo_import",
    "lo_export",
    "lo_get",
    "lo_put",
    // Network reach via FDW-ish
    "dblink",
    "dblink_connect",
    "dblink_exec",
    "dblink_send_query",
    // Backend control
    "pg_terminate_backend",
    "pg_cancel_backend",
    "pg_reload_conf",
    "pg_promote",
    "pg_logfile_rotate",
    "pg_rotate_logfile",
    // GUC poisoning across the transaction
    "set_config",
];

/// `COPY` is special: not a function, a top-level statement. We disallow it
/// outright because `COPY … FROM PROGRAM` runs shell commands and `COPY … TO
/// PROGRAM` exfiltrates data.
const BANNED_LEAD_KEYWORD: &str = "copy";

pub fn single_statement(tokens: &[Token<'_>]) -> Result<()> {
    let mut seen_non_semi_after_semi = false;
    let mut seen_semi = false;
    for t in tokens {
        if matches!(t, Token::Semicolon) {
            seen_semi = true;
            continue;
        }
        if seen_semi {
            seen_non_semi_after_semi = true;
            break;
        }
    }
    if seen_non_semi_after_semi {
        return Err(AskError::GuardRejected(
            "multi-statement payloads are not permitted".into(),
        ));
    }
    Ok(())
}

pub fn starts_with_allowed_verb(tokens: &[Token<'_>], mode: GuardMode) -> Result<()> {
    let first = first_word(tokens)
        .ok_or_else(|| AskError::GuardRejected("could not find a leading SQL verb".into()))?;
    let lower = first.to_ascii_lowercase();

    if lower == BANNED_LEAD_KEYWORD {
        return Err(AskError::GuardRejected(
            "COPY is not permitted (use SELECT instead)".into(),
        ));
    }

    let allowed = match mode {
        GuardMode::Readonly => READONLY_VERBS.contains(&lower.as_str()),
        GuardMode::Writable => {
            READONLY_VERBS.contains(&lower.as_str()) || WRITE_VERBS.contains(&lower.as_str())
        }
    };
    if !allowed {
        return Err(AskError::GuardRejected(format!(
            "leading verb `{first}` is not on the allowed list for the current mode"
        )));
    }
    Ok(())
}

pub fn no_banned_functions(tokens: &[Token<'_>]) -> Result<()> {
    // Match: Word("foo") followed by LParen → treat as function call.
    for window in tokens.windows(2) {
        if let (Token::Word(name), Token::LParen) = (&window[0], &window[1]) {
            // Strip any `schema.` prefix; banned names match the rightmost segment.
            let leaf = name.rsplit('.').next().unwrap_or(name).to_ascii_lowercase();
            if BANNED_FUNCTIONS.contains(&leaf.as_str()) {
                return Err(AskError::GuardRejected(format!(
                    "function `{name}` is not permitted"
                )));
            }
        }
    }
    Ok(())
}

fn first_word<'a>(tokens: &'a [Token<'a>]) -> Option<&'a str> {
    tokens.iter().find_map(|t| match t {
        Token::Word(w) => Some(*w),
        _ => None,
    })
}
