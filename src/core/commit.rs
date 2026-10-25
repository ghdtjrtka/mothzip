//! One owner for verified file replacement and rollback. Originals remain until finalize.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct CommitError {
    message: String,
    pub recovery_required: bool,
}

struct JournalEntry {
    /// Final target path. For a replacement this is where the staged file is installed;
    /// for a removal this is the path that was deleted.
    destination: PathBuf,
    /// Backup location where the pre-existing file was moved (None if none existed).
    original: Option<PathBuf>,
    /// True when a new file was installed at `destination` this transaction.
    installed: bool,
}

pub struct FileTransaction {
    journal: Vec<JournalEntry>,
    settled: bool,
    backup_serial: u64,
    backup_token: u64,
}
impl FileTransaction {
    pub fn apply(files: &[(PathBuf, PathBuf)]) -> Result<Self, CommitError> {
        Self::apply_with_removals(files, &[])
    }

    /// Replace `files` (staged -> destination) and remove `removals` as one atomic
    /// transaction. Every replaced/deleted original stays recoverable until `finalize`;
    /// a partial failure restores the whole set. `removals` that do not exist are
    /// tolerated; non-regular targets (directories, symlinks, reparse points) are refused.
    pub fn apply_with_removals(
        files: &[(PathBuf, PathBuf)],
        removals: &[PathBuf],
    ) -> Result<Self, CommitError> {
        let mut transaction = Self {
            journal: Vec::new(),
            settled: false,
            backup_serial: 0,
            backup_token: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0) as u64,
        };
        let attempt: io::Result<()> = (|| {
            for (staged, destination) in files {
                transaction.stage_replacement(staged, destination)?;
            }
            for path in removals {
                transaction.stage_removal(path)?;
            }
            Ok(())
        })();
        if let Err(cause) = attempt {
            let failures = transaction.restore();
            transaction.settled = true;
            return Err(CommitError {
                message: if failures.is_empty() {
                    format!("Commit failed; original files restored: {cause}")
                } else {
                    format!(
                        "Commit failed: {cause}. Recovery files retained: {}",
                        failures.join("; ")
                    )
                },
                recovery_required: !failures.is_empty(),
            });
        }
        Ok(transaction)
    }

    fn stage_replacement(&mut self, staged: &Path, destination: &Path) -> io::Result<()> {
        let staged_meta = fs::symlink_metadata(staged)?;
        if !staged_meta.is_file() || staged_meta.file_type().is_symlink() {
            return Err(io::Error::other(
                "Staged result must be an owned regular file",
            ));
        }
        let original = match fs::symlink_metadata(destination) {
            Ok(meta) => {
                if !is_regular_target(&meta) {
                    return Err(io::Error::other(format!(
                        "Destination is not a regular file: {}",
                        destination.display()
                    )));
                }
                let backup = self.next_backup(destination);
                self.expect_backup_absent(&backup)?;
                fs::rename(destination, &backup)?;
                Some(backup)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        self.journal.push(JournalEntry {
            destination: destination.to_path_buf(),
            original,
            installed: false,
        });
        fs::rename(staged, destination)?;
        self.journal
            .last_mut()
            .expect("recorded replacement")
            .installed = true;
        Ok(())
    }

    fn stage_removal(&mut self, path: &Path) -> io::Result<()> {
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if !is_regular_target(&meta) {
            return Err(io::Error::other(format!(
                "Refusing to remove non-regular target: {}",
                path.display()
            )));
        }
        let backup = self.next_backup(path);
        self.expect_backup_absent(&backup)?;
        fs::rename(path, &backup)?;
        self.journal.push(JournalEntry {
            destination: path.to_path_buf(),
            original: Some(backup),
            installed: false,
        });
        Ok(())
    }

    fn next_backup(&mut self, target: &Path) -> PathBuf {
        let serial = self.backup_serial;
        self.backup_serial += 1;
        let parent = target.parent().unwrap_or(Path::new("."));
        parent.join(format!(
            ".mothzip_bak_{}_{}_{}",
            std::process::id(),
            self.backup_token,
            serial
        ))
    }

    fn expect_backup_absent(&self, backup: &Path) -> io::Result<()> {
        match fs::symlink_metadata(backup) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                backup.display().to_string(),
            )),
        }
    }

    fn restore(&mut self) -> Vec<String> {
        let mut failures = Vec::new();
        for item in self.journal.iter().rev() {
            if item.installed {
                if let Err(e) = fs::remove_file(&item.destination) {
                    failures.push(format!("{}: {}", item.destination.display(), e));
                    continue;
                }
            }
            if let Some(original) = &item.original {
                if let Err(e) = fs::rename(original, &item.destination) {
                    failures.push(format!(
                        "{} -> {}: {}",
                        original.display(),
                        item.destination.display(),
                        e
                    ));
                }
            }
        }
        failures
    }
    pub fn rollback(mut self) -> Result<(), CommitError> {
        let failures = self.restore();
        self.settled = true;
        if failures.is_empty() {
            Ok(())
        } else {
            Err(CommitError {
                message: format!(
                    "Recovery files retained; rollback failed: {}",
                    failures.join("; ")
                ),
                recovery_required: true,
            })
        }
    }
    pub fn finalize(mut self) -> Result<(), CommitError> {
        // New results are accepted. A backup cleanup error must never trigger a partial rollback.
        self.settled = true;
        for item in &self.journal {
            if let Some(original) = &item.original {
                discard_original(original).map_err(|e| CommitError {
                    message: format!(
                        "Files committed; recovery file retained at '{}': {}",
                        original.display(),
                        e
                    ),
                    recovery_required: true,
                })?;
            }
        }
        Ok(())
    }
}
impl Drop for FileTransaction {
    fn drop(&mut self) {
        if !self.settled {
            let _ = self.restore();
        }
    }
}
pub fn commit_files(files: &[(PathBuf, PathBuf)]) -> Result<(), CommitError> {
    FileTransaction::apply(files)?.finalize()
}
pub fn commit_files_with_removals(
    files: &[(PathBuf, PathBuf)],
    removals: &[PathBuf],
) -> Result<(), CommitError> {
    FileTransaction::apply_with_removals(files, removals)?.finalize()
}

/// A removable/replaceable target must be a regular file (not a directory, symlink, or
/// reparse point). Junctions and other reparse-point objects are refused on Windows.
fn is_regular_target(meta: &fs::Metadata) -> bool {
    if !meta.is_file() || meta.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

#[cfg(not(windows))]
fn discard_original(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

/// Delete the backup directory entry while preserving the attributes of any other hardlink.
/// An unsupported operation or sharing violation is an error; never clear read-only attributes.
#[cfg(windows)]
fn discard_original(path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    if !fs::symlink_metadata(path)?.permissions().readonly() {
        return fs::remove_file(path);
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    #[repr(C)]
    struct Disposition {
        flags: u32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *mut std::ffi::c_void,
            creation: u32,
            flags: u32,
            template: *mut std::ffi::c_void,
        ) -> *mut std::ffi::c_void;
        fn SetFileInformationByHandle(
            handle: *mut std::ffi::c_void,
            class: i32,
            info: *const std::ffi::c_void,
            size: u32,
        ) -> i32;
        fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    }
    const DELETE: u32 = 0x0001_0000;
    const SHARE_ALL: u32 = 1 | 2 | 4;
    const OPEN_EXISTING: u32 = 3;
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const DISPOSITION_EX: i32 = 21;
    const DELETE_POSIX_IGNORE_READONLY: u32 = 0x01 | 0x02 | 0x10;
    // SAFETY: wide is NUL-terminated; the handle is checked, used once, then closed.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            DELETE,
            SHARE_ALL,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == -1isize as *mut std::ffi::c_void || handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let info = Disposition {
        flags: DELETE_POSIX_IGNORE_READONLY,
    };
    // SAFETY: info has the FILE_DISPOSITION_INFO_EX ABI and remains alive for the call.
    let ok = unsafe {
        SetFileInformationByHandle(
            handle,
            DISPOSITION_EX,
            &info as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<Disposition>() as u32,
        )
    };
    let error = if ok == 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };
    unsafe { CloseHandle(handle) };
    match error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
