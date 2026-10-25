//! Background catalog loads, extractions and integrity checks.
//!
//! The window stays responsive because every filesystem-heavy producer runs here, on a worker
//! thread, and reports back when it finishes. The threads share no state: the outcome crosses the
//! boundary through a channel, and the UI thread can take it even when the window is destroyed
//! before it dispatches the posted notification. Each handle lets a closing window wait for the
//! worker's commit or cleanup instead of abandoning it mid-write, and an extraction handle also
//! carries the cancellation signal the window sets when the user asks it to stop.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::core::catalog::{read_archive_catalog, CatalogEntry};
use crate::core::extract::{
    extract_archive_with_options_and_cancel, CancellationToken, ExtractionOptions,
};
use crate::core::inspect::test_archive_integrity;

/// Posted by a worker when its job finished; the outcome itself comes through the job's channel.
pub const WM_APP_JOB_DONE: u32 = WM_APP + 1;

/// Posted by the catalog worker when the archive finished reading; the entries come through the
/// load's channel, and the low word of `wParam` carries the load's generation id.
pub const WM_APP_LOAD_DONE: u32 = WM_APP + 2;

/// The worker thread names, so a hang is identifiable in a debugger or a dump.
const JOB_WORKER_NAME: &str = "mothzip-ui-job";
const LOAD_WORKER_NAME: &str = "mothzip-ui-load";

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

    /// Whether this job can be stopped before it finishes. Extraction stages its output and
    /// discards the staging on an abort, so cutting it short leaves the destination untouched;
    /// the integrity check re-reads the archive through a producer with no cancellation seam in an
    /// owned file, so offering a cancel button for it would block the window instead of stopping
    /// anything.
    pub fn cancellable(self) -> bool {
        match self {
            JobKind::ExtractAll | JobKind::ExtractSelected => true,
            JobKind::Test => false,
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
    /// The user stopped the job. This is not a failure: nothing was committed and the process
    /// still exits successfully.
    Cancelled,
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

    /// Whether the user's cancellation stopped the job.
    pub fn cancelled(&self) -> bool {
        matches!(self.result, JobResult::Cancelled)
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
    kind: JobKind,
    handle: JoinHandle<()>,
    outcomes: Receiver<JobOutcome>,
    cancellation: CancellationToken,
}

impl Job {
    /// Whether the window may offer to stop this job; see [`JobKind::cancellable`].
    pub fn cancellable(&self) -> bool {
        self.kind.cancellable()
    }

    /// Ask the producer to stop at its next checkpoint. It stops within one member or one copied
    /// chunk for an extraction, and then reports [`JobResult::Cancelled`].
    pub fn request_cancel(&self) {
        self.cancellation.cancel();
    }

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

/// Run `request` on a worker thread and return the handle that can stop or wait for it.
///
/// A failure to start the thread is returned so the caller can report it; the thread itself never
/// fails to report, because the outcome is sent even when the producer errors.
pub fn spawn(window: HWND, request: JobRequest) -> io::Result<Job> {
    let target = window as isize;
    let (sender, outcomes) = mpsc::channel();
    let cancellation = CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let kind = request.kind;
    let handle = thread::Builder::new()
        .name(JOB_WORKER_NAME.to_string())
        .spawn(move || {
            let outcome = JobOutcome {
                kind: request.kind,
                result: run(&request, &worker_cancellation),
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
    Ok(Job {
        kind,
        handle,
        outcomes,
        cancellation,
    })
}

fn run(request: &JobRequest, cancellation: &CancellationToken) -> JobResult {
    match request.kind {
        JobKind::Test => test(request),
        JobKind::ExtractAll | JobKind::ExtractSelected => extract(request, cancellation),
    }
}

fn extract(request: &JobRequest, cancellation: &CancellationToken) -> JobResult {
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
    match extract_archive_with_options_and_cancel(
        &request.archive,
        destination,
        &options,
        Some(cancellation),
    ) {
        Ok(summary) => JobResult::Extracted {
            files: summary.extracted_files,
            bytes: summary.extracted_bytes,
            target: summary.target_dir,
        },
        Err(error) if is_cancellation(error.as_ref()) => JobResult::Cancelled,
        Err(error) => JobResult::Failed(error.to_string()),
    }
}

/// Whether a producer error is the cancellation this job asked for.
///
/// The extractor reports a cancelled run as `Interrupted`, so this decision never reads message
/// text; walking the source chain also covers an error that wrapped the original one.
fn is_cancellation(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(io_error) = error.downcast_ref::<io::Error>() {
            if io_error.kind() == io::ErrorKind::Interrupted {
                return true;
            }
        }
        current = error.source();
    }
    false
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

/// What the catalog worker reads; nothing in it borrows the window.
pub struct LoadRequest {
    pub archive: PathBuf,
    pub password: Option<String>,
    pub max_bytes: u64,
}

/// A catalog read in flight, and the generation id its notification carries.
pub struct LoadJob {
    id: u64,
    handle: JoinHandle<()>,
    outcomes: Receiver<Result<Vec<CatalogEntry>, String>>,
}

impl LoadJob {
    /// The id posted with [`WM_APP_LOAD_DONE`], so a late notification from another load can be
    /// recognized as not belonging to this one.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Whether the worker already ended. Used when the window closes: a finished load is joined,
    /// a running one is detached.
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// Wait for the worker and take the entries it read.
    ///
    /// `None` means the worker ended without reporting (it panicked); the caller reports that as a
    /// load failure. `Err` carries the producer's message, which is what the status line shows.
    pub fn finish(self) -> Option<Result<Vec<CatalogEntry>, String>> {
        let joined = self.handle.join();
        if joined.is_err() {
            return None;
        }
        self.outcomes.try_recv().ok()
    }

    /// Release a worker that is still reading without waiting for it.
    ///
    /// A catalog read owns only the archive path and its own channel, so nothing else observes it
    /// after this returns; the notification it posts when it ends carries [`LoadJob::id`], which
    /// the window compares before taking an outcome.
    pub fn detach(self) {
        drop(self.handle);
    }
}

/// Generation counter for load notifications; ids only have to be unique inside one process.
static NEXT_LOAD_ID: AtomicU64 = AtomicU64::new(1);

/// Read the catalog of `request.archive` on a worker thread.
///
/// The window is already on screen when this is called, so the archive read must not run on the
/// UI thread: the entries come back through the channel, the UI thread fills the list.
pub fn spawn_load(window: HWND, request: LoadRequest) -> io::Result<LoadJob> {
    let id = NEXT_LOAD_ID.fetch_add(1, Ordering::Relaxed);
    let target = window as isize;
    let (sender, outcomes) = mpsc::channel();
    let handle = thread::Builder::new()
        .name(LOAD_WORKER_NAME.to_string())
        .spawn(move || {
            let entries = read_archive_catalog(
                &request.archive,
                request.password.as_deref(),
                request.max_bytes,
            )
            .map_err(|error| error.to_string());
            let _ = sender.send(entries);
            // SAFETY: posting a notification to the window this load was started for. The id in
            // `wParam` is what keeps a late post from being taken for another load's result.
            unsafe {
                PostMessageW(target as HWND, WM_APP_LOAD_DONE, id as usize, 0);
            }
        })?;
    Ok(LoadJob {
        id,
        handle,
        outcomes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(label: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "mothzip-job-{label}-{}-{stamp}",
            std::process::id()
        ))
    }

    /// A stored ZIP of `chunks` x 1 MiB, big enough that a cancel racing the worker still lands
    /// inside the extraction.
    fn stored_zip(dir: &Path, chunks: usize) -> PathBuf {
        let archive = dir.join("fixture.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).expect("create zip"));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let block = vec![0x5Au8; 1024 * 1024];
        for index in 0..chunks {
            writer
                .start_file(format!("chunk{index:02}.bin"), options)
                .expect("start member");
            writer.write_all(&block).expect("write member");
        }
        writer.finish().expect("finish zip");
        archive
    }

    fn staging_entries(destination: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        if let Ok(entries) = std::fs::read_dir(destination) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".mothzip_stage_")
                {
                    found.push(entry.path());
                }
            }
        }
        found
    }

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
        let outcome = Job {
            kind: JobKind::Test,
            handle,
            outcomes,
            cancellation: CancellationToken::new(),
        }
        .finish()
        .expect("the worker reported an outcome");
        assert_eq!(outcome.kind, JobKind::Test);
        assert!(matches!(outcome.result, JobResult::Tested { entries: 4 }));
        assert!(!outcome.failed());
        assert!(!outcome.cancelled());
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
        let outcome = Job {
            kind: JobKind::ExtractAll,
            handle,
            outcomes,
            cancellation: CancellationToken::new(),
        }
        .finish()
        .expect("the worker reported an outcome");
        assert!(outcome.failed());
        assert!(!outcome.cancelled());
    }

    #[test]
    fn finish_reports_a_worker_that_died_without_an_outcome_as_missing() {
        let (_sender, outcomes) = mpsc::channel::<JobOutcome>();
        let handle = thread::spawn(|| {});
        assert!(Job {
            kind: JobKind::Test,
            handle,
            outcomes,
            cancellation: CancellationToken::new(),
        }
        .finish()
        .is_none());
    }

    #[test]
    fn a_cancelled_job_is_not_a_failed_job() {
        let outcome = JobOutcome {
            kind: JobKind::ExtractAll,
            result: JobResult::Cancelled,
        };
        assert!(!outcome.failed(), "the user's own stop is not a failure");
        assert!(outcome.cancelled());
    }

    #[test]
    fn only_extraction_offers_cancellation() {
        assert!(JobKind::ExtractAll.cancellable());
        assert!(JobKind::ExtractSelected.cancellable());
        assert!(
            !JobKind::Test.cancellable(),
            "the integrity producer has no cancellation seam in an owned file"
        );
    }

    #[test]
    fn cancellation_is_recognized_by_kind_not_by_message() {
        let interrupted = io::Error::new(io::ErrorKind::Interrupted, "stopped");
        assert!(is_cancellation(&interrupted));
        let wrapped = io::Error::other("outer");
        let boxed: Box<dyn std::error::Error> = Box::new(wrapped);
        assert!(!is_cancellation(boxed.as_ref()));
        // A real error that happens to mention the word is still a failure.
        let misleading = io::Error::other("Interrupted by a signal from the disk driver");
        assert!(!is_cancellation(&misleading));
    }

    #[test]
    fn a_cancelled_extraction_reports_cancelled_and_leaves_no_output() {
        let dir = unique_dir("cancel");
        std::fs::create_dir_all(&dir).expect("create the fixture directory");
        let archive = stored_zip(&dir, 8);
        let destination = dir.join("out");

        let request = JobRequest {
            kind: JobKind::ExtractAll,
            archive: archive.clone(),
            password: None,
            max_bytes: 1 << 30,
            destination: Some(destination.clone()),
            members: Vec::new(),
        };
        // No window: posting the completion notification to a null handle fails harmlessly, and
        // the outcome still arrives through the channel.
        let job = spawn(std::ptr::null_mut(), request).expect("spawn the worker");
        assert!(job.cancellable());
        job.request_cancel();
        let outcome = job.finish().expect("the worker reported an outcome");
        assert!(
            outcome.cancelled(),
            "a cancelled worker must report cancellation"
        );
        assert!(!outcome.failed(), "cancellation is not a failure exit");
        assert!(
            !destination.exists() || std::fs::read_dir(&destination).unwrap().next().is_none(),
            "a cancelled extraction must not leave committed files"
        );
        assert!(
            staging_entries(&destination).is_empty(),
            "a cancelled extraction must remove its staging directory"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_load_worker_reports_the_catalog_and_a_failure_as_its_own_outcome() {
        let dir = unique_dir("load");
        std::fs::create_dir_all(&dir).expect("create the fixture directory");
        let archive = stored_zip(&dir, 1);

        let loaded = spawn_load(
            std::ptr::null_mut(),
            LoadRequest {
                archive: archive.clone(),
                password: None,
                max_bytes: 1 << 30,
            },
        )
        .expect("spawn the loader");
        let entries = loaded
            .finish()
            .expect("the loader reported an outcome")
            .expect("the fixture archive reads");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "chunk00.bin");

        let missing = spawn_load(
            std::ptr::null_mut(),
            LoadRequest {
                archive: dir.join("absent.zip"),
                password: None,
                max_bytes: 1 << 30,
            },
        )
        .expect("spawn the loader");
        let failure = missing
            .finish()
            .expect("the loader reported an outcome")
            .expect_err("a missing archive cannot be read");
        assert!(!failure.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_generation_ids_are_unique_so_a_late_notification_can_be_rejected() {
        let first = spawn_load(
            std::ptr::null_mut(),
            LoadRequest {
                archive: PathBuf::from("first.zip"),
                password: None,
                max_bytes: 1 << 20,
            },
        )
        .expect("spawn the loader");
        let second = spawn_load(
            std::ptr::null_mut(),
            LoadRequest {
                archive: PathBuf::from("second.zip"),
                password: None,
                max_bytes: 1 << 20,
            },
        )
        .expect("spawn the loader");
        assert_ne!(first.id(), second.id());
        let first_id = first.id();
        let second_id = second.id();
        first.detach();
        second.detach();
        assert!(first_id < second_id);
    }
}
