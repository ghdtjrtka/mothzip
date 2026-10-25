use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ShellError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("System command failed: '{command}', exit_code: {exit_code:?}, stderr: {stderr}")]
    SystemCommand {
        command: String,
        exit_code: Option<i32>,
        stderr: String,
    },

    #[error("Registry operation failed: '{command}', exit_code: {exit_code:?}, stderr: {stderr}")]
    Registry {
        command: String,
        exit_code: Option<i32>,
        stderr: String,
    },

    #[error("Shortcut operation failed for '{target}': {message}")]
    Shortcut { target: PathBuf, message: String },

    #[allow(dead_code)]
    #[error("User PATH update failed: {message}")]
    PathEnv { message: String },

    #[error("Manifest error at '{path}': {message}")]
    Manifest { path: PathBuf, message: String },

    #[error("Validation failed for '{field}': {message}")]
    Validation { field: String, message: String },

    #[error("Security violation for '{path}': {reason}")]
    SecurityViolation { path: PathBuf, reason: String },

    #[error("Cleanup helper execution failed: {message}")]
    CleanupHelperFailed { message: String },
}
