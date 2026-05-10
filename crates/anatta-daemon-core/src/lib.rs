//! Daemon-side library for anatta.
//!
//! Drives the intent state machine, owns the bidi gRPC stream with the
//! server, and orchestrates calls into `anatta-worktree`,
//! `anatta-runtime`, and (in Phase 2) `anatta-guards`.
//!
//! The `apps/anatta-daemon` binary is a thin wrapper that parses
//! configuration and calls into this crate.
