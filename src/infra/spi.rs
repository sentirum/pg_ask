//! Tiny SPI helpers used across the crate.
//!
//! Centralising these means tool / schema / telemetry code never repeats the
//! same `Spi::connect → get_datum_by_ordinal → value::<T>` dance, and that
//! `From<SpiError>` keeps the error chain clean.
//!
//! Most callers only need [`select_one_text_with`] today; the no-arg helpers
//! land alongside their first consumer in v0.2 (sessions / telemetry).

use crate::infra::errors::Result;
use pgrx::prelude::*;

/// Run a parameterised `SELECT` that returns a single `text` value (or NULL).
pub fn select_one_text_with<'mcx>(
    query: &str,
    args: &[pgrx::datum::DatumWithOid<'mcx>],
) -> Result<Option<String>> {
    Ok(Spi::get_one_with_args::<String>(query, args)?)
}
