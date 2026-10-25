use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

#[derive(Debug, Clone, serde::Serialize)]
pub struct PackSummary {
    pub archive_path: PathBuf,
    pub total_files: usize,
    pub total_uncompressed_bytes: u64,
    pub compressed_bytes: u64,
    pub compression_ratio: String,
}

/// Compress files or directories into a `.zip` archive.
pub fn pack_archive(
    dest_zip: &Path,
    sources: &[PathBuf],
) -> Result<PackSummary, Box<dyn std::error::Error>> {
    if let Some(parent) = dest_zip.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let file = File::create(dest_zip)?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o755);

    let mut total_files = 0;
    let mut total_uncompressed_bytes = 0u64;
    let mut buffer = Vec::new();

    for source in sources {
        if source.is_file() {
            let file_name = source
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file");

            zip.start_file(file_name, options)?;
            let mut f = File::open(source)?;
            buffer.clear();
            f.read_to_end(&mut buffer)?;
            zip.write_all(&buffer)?;

            total_uncompressed_bytes += buffer.len() as u64;
            total_files += 1;
        } else if source.is_dir() {
            let base_parent = source.parent().unwrap_or(source);

            for entry in WalkDir::new(source).into_iter().filter_map(|e| e.ok()) {
                let path = entry.path();
                let relative = path.strip_prefix(base_parent)?;
                let name = relative.to_string_lossy().replace('\\', "/");

                if name.is_empty() {
                    continue;
                }

                if entry.file_type().is_dir() {
                    zip.add_directory(format!("{}/", name), options)?;
                } else if entry.file_type().is_file() {
                    zip.start_file(name, options)?;
                    let mut f = File::open(path)?;
                    buffer.clear();
                    f.read_to_end(&mut buffer)?;
                    zip.write_all(&buffer)?;

                    total_uncompressed_bytes += buffer.len() as u64;
                    total_files += 1;
                }
            }
        }
    }

    let finished_file = zip.finish()?;
    let compressed_bytes = finished_file.metadata()?.len();

    let ratio_num = if compressed_bytes > 0 {
        total_uncompressed_bytes as f64 / compressed_bytes as f64
    } else {
        1.0
    };
    let compression_ratio = format!("{:.1}x", ratio_num);

    Ok(PackSummary {
        archive_path: dest_zip.to_path_buf(),
        total_files,
        total_uncompressed_bytes,
        compressed_bytes,
        compression_ratio,
    })
}
