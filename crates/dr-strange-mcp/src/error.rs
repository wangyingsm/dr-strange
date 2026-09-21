//! What this crate refuses to start on.
//!
//! The stdio binary has no config file, so every knob it takes arrives as an
//! environment variable. A value that is not a number is a mistake worth
//! naming: the variable, what it held, and what a valid one looks like.

use crate::{ENV_RETAIN_COMMITS, ENV_TOOL_DEADLINE_SECS};

/// A setting this crate could not read.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum McpError {
    /// [`ENV_RETAIN_COMMITS`] held something that is not a count.
    #[error(
        "{ENV_RETAIN_COMMITS}={value:?} is not a number of commits \
         (a whole number; 0 keeps every version)"
    )]
    RetainCommits { value: String },

    /// [`ENV_TOOL_DEADLINE_SECS`] held something that is not a count of
    /// seconds.
    #[error(
        "{ENV_TOOL_DEADLINE_SECS}={value:?} is not a number of seconds \
         (a whole number; 0 removes the deadline)"
    )]
    ToolDeadline { value: String },

    /// What goes back to the agent over the wire. Wrapped rather than kept
    /// apart so one type crosses the whole crate: a tool body that fails on
    /// a setting and one that fails on a request are the same kind of thing
    /// to everything between here and the transport.
    #[error(transparent)]
    Protocol(#[from] rmcp::ErrorData),
}

impl From<McpError> for rmcp::ErrorData {
    /// The transport speaks only its own error, so this is where a config
    /// failure becomes one the agent can read.
    fn from(e: McpError) -> Self {
        match e {
            McpError::Protocol(data) => data,
            other => rmcp::ErrorData::internal_error(other.to_string(), None),
        }
    }
}
