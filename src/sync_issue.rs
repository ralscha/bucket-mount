//! Actionable sync failures, kept separately from the short connection status.
use std::{path::PathBuf, time::Instant};

#[derive(Debug, thiserror::Error)]
#[error("{action}: HRESULT 0x{code:08X}")]
pub struct WindowsFailure {
    pub action: String,
    pub code: u32,
}

#[derive(Debug, thiserror::Error)]
#[error("{operation}: {}", path.display())]
pub struct FileOperation {
    pub path: PathBuf,
    pub operation: String,
    #[source]
    source: anyhow::Error,
}

impl FileOperation {
    pub fn wrap(
        path: impl Into<PathBuf>,
        operation: impl Into<String>,
        source: anyhow::Error,
    ) -> anyhow::Error {
        anyhow::Error::new(Self {
            path: path.into(),
            operation: operation.into(),
            source,
        })
    }
}

pub fn is_busy(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<WindowsFailure>()
            .is_some_and(|e| matches!(e.code, 0x80070020 | 0x80070021))
            || cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| matches!(e.raw_os_error(), Some(32 | 33)))
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncIssue {
    pub path: Option<PathBuf>,
    pub operation: String,
    pub busy: bool,
    pub details: String,
    pub first_seen: Instant,
}

impl SyncIssue {
    pub fn from_error(error: &anyhow::Error, path: Option<PathBuf>, operation: &str) -> Self {
        // The innermost path identifies the actual lock, which may be a parent folder.
        let context = error
            .chain()
            .filter_map(|e| e.downcast_ref::<FileOperation>())
            .last();
        Self {
            path: context.map(|c| c.path.clone()).or(path),
            operation: context.map_or_else(|| operation.to_owned(), |c| c.operation.clone()),
            busy: is_busy(error),
            details: format!("{error:#}"),
            first_seen: Instant::now(),
        }
    }

    pub fn same_item(&self, other: &Self) -> bool {
        self.path == other.path && self.operation == other.operation && self.busy == other.busy
    }

    pub fn guidance(&self) -> &'static str {
        if self.busy {
            "Another application is using this file or folder. Save your work and close the application to let synchronization continue."
        } else {
            "This item could not synchronize. Review the technical details, correct the problem, then retry."
        }
    }

    pub fn copy_text(&self) -> String {
        format!(
            "{}\r\n{}\r\n{}\r\n\r\n{}",
            self.path
                .as_ref()
                .map_or_else(|| "Connection".into(), |p| p.display().to_string()),
            self.operation,
            self.guidance(),
            self.details
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_errors_keep_the_actual_parent_path_through_context() {
        let error = anyhow::Error::new(WindowsFailure {
            action: "lock cloud file".into(),
            code: 0x80070020,
        });
        let error = FileOperation::wrap(r"C:\mount\folder", "Prepare folder", error);
        let error = FileOperation::wrap(r"C:\mount\folder\file.txt", "Synchronize file", error);
        let issue = SyncIssue::from_error(&error, None, "Sync");
        assert!(issue.busy);
        assert_eq!(issue.path, Some(PathBuf::from(r"C:\mount\folder")));
        assert!(issue.details.contains("0x80070020"));
        assert!(is_busy(&anyhow::Error::new(
            std::io::Error::from_raw_os_error(33)
        )));
        assert!(!is_busy(&anyhow::anyhow!(
            "server returned 0x80070020 in its response"
        )));
        assert!(!is_busy(&anyhow::Error::new(
            std::io::Error::from_raw_os_error(5)
        )));
    }
}
