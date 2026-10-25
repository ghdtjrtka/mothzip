//! Background extraction and integrity checks.
//!
//! The window stays responsive because every filesystem-heavy producer runs here, on one worker
//! thread, and reports back with a single posted message. The threads share no state: the outcome
//! crosses the boundary as one boxed value that the UI thread takes ownership of.

use std::path::PathBuf;
use std::thread;

use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::core::extract::{extract_archive_with_options, ExtractionOptions};
use crate::core::inspect::test_archive_integrity;

/// Posted by a worker when its job finished; `lparam` carries one boxed [`JobOutcome`].
pub const WM_APP_JOB_DONE: u32 = WM_APP + 1;

/// The worker thread's name, so a hang is identifiable in a debugger or a dump.
const WORKER_NAME: &str = "mothzip-ui-job";

/// What the user asked the worker to do.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    ExtractAll,
    ExtractSelected,
    Test,
}

impl JobKind {
    /// Status line while the job runs; the ellipsis is the only progress signal, because the
    /// producers report totals only when they finish.
    pub fn running_label(self) -> &'static str {
        match self {
            JobKind::ExtractAll | JobKind::ExtractSelected => "푸는 중…",
            JobKind::Test => "검사 중…",
        }
    }

    /// Status line prefix when the job fails.
    pub fn failure_label(self) -> &'static str {
        match self {
            JobKind::ExtractAll | JobKind::ExtractSelected => "압축 풀기 실패",
            JobKind::Test => "검사 실패",
        }
    }
}

/// What the producer reported, already reduced to what the status line shows.
pub enum JobResult {
    Extracted {
        files: usize,
        bytes: u64,
        target: PathBuf,
    },
    Tested {
        entries: usize,
    },
    Failed(String),
}

/// One finished job, handed from the worker thread to the UI thread.
pub struct JobOutcome {
    pub kind: JobKind,
    pub result: JobResult,
}

/// Everything the worker needs; nothing in it borrows the window.
pub struct JobRequest {
    pub kind: JobKind,
    pub archive: PathBuf,
    pub password: Option<String>,
    pub max_bytes: u64,
    /// Chosen output folder; only integrity checks run without one.
    pub destination: Option<PathBuf>,
    /// Literal member names for a partial extraction; empty extracts everything.
    pub members: Vec<String>,
}

/// Run `request` on a worker thread and post its outcome to `window`.
///
/// A failure to start the thread is returned so the caller can report it; the thread itself never
/// fails to report, because the outcome is posted even when the producer errors.
pub fn spawn(window: HWND, request: JobRequest) -> std::io::Result<()> {
    let target = window as isize;
    thread::Builder::new()
        .name(WORKER_NAME.to_string())
        .spawn(move || {
            let outcome = JobOutcome {
                kind: request.kind,
                result: run(&request),
            };
            let payload = Box::into_raw(Box::new(outcome));
            // SAFETY: `payload` was just allocated for this message; if the post fails (the
            // window is already gone) we still own it and free it below instead of leaking.
            let posted =
                unsafe { PostMessageW(target as HWND, WM_APP_JOB_DONE, 0, payload as LPARAM) };
            if posted == 0 {
                // SAFETY: `payload` was not transferred and is dropped exactly once here.
                drop(unsafe { Box::from_raw(payload) });
            }
        })?;
    Ok(())
}

fn run(request: &JobRequest) -> JobResult {
    match request.kind {
        JobKind::Test => test(request),
        JobKind::ExtractAll | JobKind::ExtractSelected => extract(request),
    }
}

fn extract(request: &JobRequest) -> JobResult {
    let Some(destination) = request.destination.as_ref() else {
        return JobResult::Failed("풀 대상 폴더가 선택되지 않았습니다".to_string());
    };
    let options = ExtractionOptions {
        smart: false,
        to_folder: false,
        password: request.password.clone(),
        max_bytes: request.max_bytes,
        members: request.members.clone(),
        flat: false,
    };
    match extract_archive_with_options(&request.archive, destination, &options) {
        Ok(summary) => JobResult::Extracted {
            files: summary.extracted_files,
            bytes: summary.extracted_bytes,
            target: summary.target_dir,
        },
        Err(error) => JobResult::Failed(error.to_string()),
    }
}

fn test(request: &JobRequest) -> JobResult {
    match test_archive_integrity(
        &request.archive,
        request.password.as_deref(),
        request.max_bytes,
    ) {
        Ok(summary) if summary.passed => JobResult::Tested {
            entries: summary.tested_entries,
        },
        Ok(summary) => JobResult::Failed(
            summary
                .error_message
                .unwrap_or_else(|| "무결성 검사가 통과하지 못했습니다".to_string()),
        ),
        Err(error) => JobResult::Failed(error.to_string()),
    }
}
