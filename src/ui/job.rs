//! Background extraction and integrity checks.
//!
//! The window stays responsive because every filesystem-heavy producer runs here, on one worker
//! thread, and reports back when it finishes. The threads share no state: the outcome crosses the
//! boundary through a channel, and the UI thread can take it even when the window is destroyed
//! before it dispatches the posted notification. The handle lets a closing window wait for the
//! worker's commit or cleanup instead of abandoning it mid-write.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::core::extract::{extract_archive_with_options, ExtractionOptions};
use crate::core::inspect::test_archive_integrity;

/// Posted by a worker when its job finished; the outcome itself comes through the job's channel.
pub const WM_APP_JOB_DONE: u32 = WM_APP + 1;

/// The worker thread's name, so a hang is identifiable in a debugger or a dump.
const WORKER_NAME: &str = "mothzip-ui-job";

/// What the user asked the worker to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
            JobKind::ExtractAll | JobKind::ExtractSelected => "Extracting…",
            JobKind::Test => "Testing…",
        }
    }

    /// Status line prefix when the job fails.
    pub fn failure_label(self) -> &'static str {
        match self {
            JobKind::ExtractAll | JobKind::ExtractSelected => "Extraction failed",
            JobKind::Test => "Test failed",
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

impl JobOutcome {
    /// Whether the producer reported an error; the window turns this into its exit code.
    pub fn failed(&self) -> bool {
        matches!(self.result, JobResult::Failed(_))
    }
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

/// A running worker and the channel through which its single outcome comes back.
pub struct Job {
    handle: JoinHandle<()>,
    outcomes: Receiver<JobOutcome>,
}

impl Job {
    /// Wait for the worker to finish and take the outcome it reported.
    ///
    /// `None` means the worker ended without reporting (it panicked), which the caller counts as a
    /// failed job. Waiting is what lets a closing window finish the worker's commit or cleanup
    /// instead of abandoning it mid-write.
    pub fn finish(self) -> Option<JobOutcome> {
        let joined = self.handle.join();
        if joined.is_err() {
            return None;
        }
        self.outcomes.try_recv().ok()
    }
}

/// Run `request` on a worker thread and return the handle that can wait for it.
///
/// A failure to start the thread is returned so the caller can report it; the thread itself never
/// fails to report, because the outcome is sent even when the producer errors.
pub fn spawn(window: HWND, request: JobRequest) -> std::io::Result<Job> {
    let target = window as isize;
    let (sender, outcomes) = mpsc::channel();
    let handle = thread::Builder::new()
        .name(WORKER_NAME.to_string())
        .spawn(move || {
            let outcome = JobOutcome {
                kind: request.kind,
                result: run(&request),
            };
            // The outcome travels through the channel rather than a message payload: a window that
            // is destroyed before dispatching WM_APP_JOB_DONE can still take it after joining.
            let _ = sender.send(outcome);
            // SAFETY: posting a payload-free notification to the window this job was started for.
            // A failed post only means the window is gone; the outcome stays in the channel.
            unsafe {
                PostMessageW(target as HWND, WM_APP_JOB_DONE, 0, 0);
            }
        })?;
    Ok(Job { handle, outcomes })
}

fn run(request: &JobRequest) -> JobResult {
    match request.kind {
        JobKind::Test => test(request),
        JobKind::ExtractAll | JobKind::ExtractSelected => extract(request),
    }
}

fn extract(request: &JobRequest) -> JobResult {
    let Some(destination) = request.destination.as_ref() else {
        return JobResult::Failed("No destination folder was chosen".to_string());
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
                .unwrap_or_else(|| "The integrity check did not pass".to_string()),
        ),
        Err(error) => JobResult::Failed(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_takes_the_outcome_after_the_worker_exits() {
        let (sender, outcomes) = mpsc::channel();
        let handle = thread::spawn(move || {
            sender
                .send(JobOutcome {
                    kind: JobKind::Test,
                    result: JobResult::Tested { entries: 4 },
                })
                .expect("the test keeps the receiver alive");
        });
        let outcome = Job { handle, outcomes }
            .finish()
            .expect("the worker reported an outcome");
        assert_eq!(outcome.kind, JobKind::Test);
        assert!(matches!(outcome.result, JobResult::Tested { entries: 4 }));
        assert!(!outcome.failed());
    }

    #[test]
    fn finish_reports_a_failed_job_as_failure() {
        let (sender, outcomes) = mpsc::channel();
        let handle = thread::spawn(move || {
            sender
                .send(JobOutcome {
                    kind: JobKind::ExtractAll,
                    result: JobResult::Failed("bad archive".to_string()),
                })
                .expect("the test keeps the receiver alive");
        });
        let outcome = Job { handle, outcomes }
            .finish()
            .expect("the worker reported an outcome");
        assert!(outcome.failed());
    }

    #[test]
    fn finish_reports_a_worker_that_died_without_an_outcome_as_missing() {
        let (_sender, outcomes) = mpsc::channel::<JobOutcome>();
        let handle = thread::spawn(|| {});
        assert!(Job { handle, outcomes }.finish().is_none());
    }
}
