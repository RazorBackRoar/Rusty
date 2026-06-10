use std::path::PathBuf;

use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("SQLite error: {0}")]
    Sql(#[from] rusqlite::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("path is outside any scanned root: {0}")]
    PathOutsideRoot(PathBuf),

    #[error("refusing to delete only remaining copy of group {0}")]
    WouldDeleteUniqueCopy(String),

    #[error("scan already running")]
    ScanAlreadyRunning,

    #[error("no scan results available to act on")]
    NoPendingPlan,

    #[error("user did not confirm destructive action")]
    NotConfirmed,

    #[error("invalid input: {0}")]
    BadInput(String),
}

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, Serialize)]
pub struct WireError {
    pub kind: String,
    pub message: String,
}

impl From<AppError> for WireError {
    fn from(value: AppError) -> Self {
        let kind = match &value {
            AppError::Io(_) => "io",
            AppError::Sql(_) => "sql",
            AppError::Json(_) => "json",
            AppError::PathOutsideRoot(_) => "path_outside_root",
            AppError::WouldDeleteUniqueCopy(_) => "would_delete_unique",
            AppError::ScanAlreadyRunning => "scan_running",
            AppError::NoPendingPlan => "no_pending_plan",
            AppError::NotConfirmed => "not_confirmed",
            AppError::BadInput(_) => "bad_input",
        };
        WireError {
            kind: kind.to_string(),
            message: value.to_string(),
        }
    }
}

impl serde::Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        WireError::from(self.clone_for_wire()).serialize(serializer)
    }
}

impl AppError {
    fn clone_for_wire(&self) -> AppError {
        match self {
            AppError::Io(e) => AppError::Io(std::io::Error::new(e.kind(), e.to_string())),
            AppError::Sql(e) => AppError::BadInput(format!("sql: {e}")),
            AppError::Json(e) => AppError::BadInput(format!("json: {e}")),
            AppError::PathOutsideRoot(p) => AppError::PathOutsideRoot(p.clone()),
            AppError::WouldDeleteUniqueCopy(s) => AppError::WouldDeleteUniqueCopy(s.clone()),
            AppError::ScanAlreadyRunning => AppError::ScanAlreadyRunning,
            AppError::NoPendingPlan => AppError::NoPendingPlan,
            AppError::NotConfirmed => AppError::NotConfirmed,
            AppError::BadInput(s) => AppError::BadInput(s.clone()),
        }
    }
}
