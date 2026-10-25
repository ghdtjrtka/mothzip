//! ZIP self-extractors reuse the normal executable and its verified extraction path.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SIGNATURE: &[u8] = b"MOTHZIP-SFX\0\x01";
const LENGTH_FIELDS: usize = 2 * size_of::<u64>();
const FOOTER_BYTES: usize = LENGTH_FIELDS + SIGNATURE.len();

#[derive(Debug, Clone, Copy)]
pub struct EmbeddedArchive {
    pub stub_bytes: u64,
    pub payload_bytes: u64,
}

/// Read our format identity and require exact containment of the embedded ZIP.
/// Ordinary executables return None; malformed signed footers are errors.
pub fn embedded_archive(path: &Path) -> io::Result<Option<EmbeddedArchive>> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    if length < FOOTER_BYTES as u64 {
        return Ok(None);
    }
    file.seek(SeekFrom::End(-(FOOTER_BYTES as i64)))?;
    let mut footer = [0u8; FOOTER_BYTES];
    file.read_exact(&mut footer)?;
    if &footer[LENGTH_FIELDS..] != SIGNATURE {
        return Ok(None);
    }
    let stub_bytes = u64::from_le_bytes(footer[..8].try_into().expect("u64 field"));
    let payload_bytes = u64::from_le_bytes(footer[8..LENGTH_FIELDS].try_into().expect("u64 field"));
    if stub_bytes == 0
        || payload_bytes == 0
        || stub_bytes
            .checked_add(payload_bytes)
            .and_then(|n| n.checked_add(FOOTER_BYTES as u64))
            != Some(length)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid MothZip self-extractor boundaries",
        ));
    }
    Ok(Some(EmbeddedArchive {
        stub_bytes,
        payload_bytes,
    }))
}

#[derive(Debug, serde::Serialize)]
pub struct SfxSummary {
    pub output_path: PathBuf,
    pub embedded_archive_bytes: u64,
    pub stub_bytes: u64,
    pub executable_bytes: u64,
    pub total_entries: usize,
}

/// Convert a ZIP into a self-extracting executable. No archived member is executed.
/// The completed replacement is tested through the same reader before committing.
pub fn create_self_extractor(
    output: &Path,
    archive: &Path,
    password: Option<&str>,
    max_bytes: u64,
) -> Result<SfxSummary, Box<dyn std::error::Error>> {
    if !output
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
    {
        return Err("Self-extractor output must have an .exe extension".into());
    }
    if super::format::detect_archive_format(archive)? != super::format::ArchiveFormat::Zip {
        return Err("Self-extractor creation currently accepts ZIP input".into());
    }
    let stub = std::env::current_exe()?;
    if let Ok(existing) = output.canonicalize() {
        if existing == archive.canonicalize()? || existing == stub.canonicalize()? {
            return Err(
                "Self-extractor output must differ from its input and running executable".into(),
            );
        }
    }
    let verification = super::inspect::test_archive_integrity(archive, password, max_bytes)?;
    if !verification.passed {
        return Err(verification
            .error_message
            .unwrap_or_else(|| "ZIP integrity check failed".into())
            .into());
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let token = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let staged = parent.join(format!(".mothzip_sfx_{}_{}.exe", std::process::id(), token));
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)?;
    let result = (|| {
        let mut source = File::open(archive)?;
        let source_length = source.metadata()?.len();
        let input = embedded_archive(archive)?;
        let payload_bytes = input.map_or(source_length, |info| info.payload_bytes);
        source.seek(SeekFrom::Start(input.map_or(0, |info| info.stub_bytes)))?;
        let executable = File::open(&stub)?;
        let stub_bytes =
            embedded_archive(&stub)?.map_or(executable.metadata()?.len(), |info| info.stub_bytes);
        if io::copy(&mut executable.take(stub_bytes), &mut target)? != stub_bytes {
            return Err("Executable changed while creating self-extractor".into());
        }
        if io::copy(&mut source.take(payload_bytes), &mut target)? != payload_bytes {
            return Err("ZIP changed while creating self-extractor".into());
        }
        target.write_all(&stub_bytes.to_le_bytes())?;
        target.write_all(&payload_bytes.to_le_bytes())?;
        target.write_all(SIGNATURE)?;
        target.sync_all()?;
        let executable_bytes = target.metadata()?.len();
        drop(target);
        let verified = super::inspect::test_archive_integrity(&staged, password, max_bytes)?;
        if !verified.passed {
            return Err(verified
                .error_message
                .unwrap_or_else(|| "Self-extractor validation failed".into())
                .into());
        }
        super::commit::commit_files(&[(staged.clone(), output.to_path_buf())])?;
        Ok(SfxSummary {
            output_path: output.to_path_buf(),
            embedded_archive_bytes: payload_bytes,
            stub_bytes,
            executable_bytes,
            total_entries: verified.total_entries,
        })
    })();
    if result.is_err() && staged.exists() {
        let _ = fs::remove_file(&staged);
    }
    result
}
