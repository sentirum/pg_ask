//! Multi-turn session storage — v0.2.
//!
//! The bootstrap SQL already provisions `pg_ask._sessions` and
//! `pg_ask._messages`. v0.2 will add:
//!
//! * an `owner` column with a check on every `chat()` call,
//! * a `chat(session_id, message)` entry point that resumes history,
//! * per-session `_config` override columns.
//!
//! Kept as an empty module today so the layering in `lib.rs` is stable.
