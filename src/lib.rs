//! # pg_ask
//!
//! PostgreSQL extension that runs an LLM-driven agent loop **inside** the database.
//!
//! Ask your DB a natural-language question; the agent reads the schema, plans SQL,
//! executes it via SPI in the same transaction, and synthesises an answer.
//!
//! ```sql
//! CREATE EXTENSION pg_ask;
//! SELECT pg_ask.config('provider',     'anthropic');
//! SELECT pg_ask.config('api_key',      'sk-ant-...');
//! SELECT pg_ask.config('model',        'claude-sonnet-4-5');
//!
//! SELECT pg_ask.ask('How many orders shipped last week?');
//! SELECT pg_ask.sql('top 5 customers by revenue');  -- generate only, no execute
//! ```

use pgrx::prelude::*;

::pgrx::pg_module_magic!();

mod agent;
mod config;
mod error;
mod providers;
mod schema;
mod session;
mod tools;

// Re-export the public SQL surface. Each module owns its own `#[pg_extern]`s.
pub use agent::*;
pub use config::*;
pub use session::*;

extension_sql_file!("../sql/bootstrap.sql", name = "bootstrap", bootstrap);

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn smoke_extension_loads() {
        // The extension loads if we got this far.
        assert_eq!(Spi::get_one::<i32>("SELECT 1").unwrap(), Some(1));
    }
}

/// pgrx test harness entry point.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![]
    }
}
