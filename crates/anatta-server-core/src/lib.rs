//! Server-side library for anatta.
//!
//! Contains the tonic service implementations, the persistence layer,
//! and the auth interceptor. The `apps/anatta-server` binary is a thin
//! wrapper that parses configuration and calls into this crate.
//!
//! Phase 1 stores state in memory; Phase 2 introduces SQLite via sqlx.
