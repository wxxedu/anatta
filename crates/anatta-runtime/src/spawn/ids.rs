//! Typed session identifiers, distinct per backend.
//!
//! Both happen to be UUIDs on the wire (claude session UUIDs, codex
//! thread UUIDs), but the type wrapper exists for the same reason as
//! `ClaudeProfileId` / `CodexProfileId` — to prevent passing the wrong
//! kind to the wrong API at compile time.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClaudeSessionId(String);

impl ClaudeSessionId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ClaudeSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CodexThreadId(String);

impl CodexThreadId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CodexThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
