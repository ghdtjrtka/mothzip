use clap::{Parser, Subcommand};
use std::path::PathBuf;

use crate::core::format::ArchiveFormat;

/// Archive format chosen by `pack-auto --format`, i.e. one entry of the Explorer
/// "압축하기" submenu. The suffix is the archive name extension written beside the selection.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackFormat {
    #[value(name = "zip")]
    Zip,
    #[value(name = "7z")]
    SevenZ,
    #[value(name = "tar")]
    Tar,
    #[value(name = "tar.gz")]
    TarGz,
    #[value(name = "tar.zst")]
    TarZst,
    #[value(name = "tar.xz")]
    TarXz,
    #[value(name = "tar.lz4")]
    TarLz4,
    #[value(name = "tar.br")]
    TarBr,
    #[value(name = "gz")]
    Gz,
    #[value(name = "zst")]
    Zst,
    #[value(name = "xz")]
    Xz,
    #[value(name = "lz4")]
    Lz4,
    #[value(name = "br")]
    Br,
}

impl PackFormat {
    /// Archive name suffix, without the leading dot.
    pub fn suffix(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::SevenZ => "7z",
            Self::Tar => "tar",
            Self::TarGz => "tar.gz",
            Self::TarZst => "tar.zst",
            Self::TarXz => "tar.xz",
            Self::TarLz4 => "tar.lz4",
            Self::TarBr => "tar.br",
            Self::Gz => "gz",
            Self::Zst => "zst",
            Self::Xz => "xz",
            Self::Lz4 => "lz4",
            Self::Br => "br",
        }
    }

    /// Container and codec this suffix selects.
    pub fn archive_format(self) -> ArchiveFormat {
        crate::core::format::from_suffix(self.suffix())
            .expect("every PackFormat suffix is registered in NAME_SUFFIXES")
    }
}

#[derive(Parser, Debug)]
#[command(name = "mothzip")]
#[command(author = "mothzip contributors")]
#[command(version)]
#[command(
    about = "Ad-free archive CLI with JSON output and Windows Explorer integration",
    long_about = None
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Output results in structured JSON format (optimized for AI Agents)
    #[arg(long, global = true)]
    pub json: bool,

    /// Suppress non-essential output
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Password for encrypted archive operations (prefer --password-stdin in automation)
    #[arg(short, long, global = true, conflicts_with = "password_stdin")]
    pub password: Option<String>,

    /// Read one password line from stdin; never prompt
    #[arg(long, global = true)]
    pub password_stdin: bool,

    /// Disable all interactive prompts and desktop windows
    #[arg(long, global = true)]
    pub non_interactive: bool,

    /// Maximum decompressed bytes per operation; increase explicitly for large archives
    #[arg(long, global = true, default_value_t = crate::core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES, value_parser = clap::value_parser!(u64).range(1..))]
    pub max_bytes: u64,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Smart unpack: automatically detects single root folder vs loose files
    #[command(name = "smart-unpack")]
    SmartUnpack {
        /// Archive path (ZIP, 7z, RAR, TAR, TAR+codec, or single stream)
        archive: PathBuf,
        /// Destination directory (defaults to archive's parent folder)
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Force extracting into a dedicated subfolder named after the archive
        #[arg(long)]
        to_folder: bool,
        /// Extract only these literal member paths or directory subtrees (repeatable)
        #[arg(long = "member", value_name = "PATH")]
        members: Vec<String>,
        /// Write files directly into the output directory without archive subdirectories
        #[arg(long)]
        flat: bool,
        /// Open destination folder in Windows Explorer upon successful completion
        #[arg(long, conflicts_with_all = ["json", "non_interactive"])]
        open: bool,
    },

    /// Direct unpack: extracts directly into specified destination
    #[command(name = "unpack")]
    Unpack {
        /// Archive path (ZIP, 7z, RAR, TAR, TAR+codec, or single stream)
        archive: PathBuf,
        /// Destination directory (defaults to the archive's parent folder)
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Force extracting into a dedicated subfolder named after the archive
        #[arg(long)]
        to_folder: bool,
        /// Extract only these literal member paths or directory subtrees (repeatable)
        #[arg(long = "member", value_name = "PATH")]
        members: Vec<String>,
        /// Write files directly into the output directory without archive subdirectories
        #[arg(long)]
        flat: bool,
        /// Open destination folder in Windows Explorer upon successful completion
        #[arg(long, conflicts_with_all = ["json", "non_interactive"])]
        open: bool,
    },

    /// Create a ZIP, 7z, TAR, TAR.<codec>, or single-file stream archive
    #[command(name = "pack")]
    Pack {
        /// Output archive path; its extension selects the format
        /// (.zip, .7z, .tar, .tar.gz/.zst/.xz/.lz4/.br, .gz/.zst/.xz/.lz4/.br)
        output: PathBuf,
        /// Files or directories to compress (omitted with --from-stdin)
        #[arg(required_unless_present = "from_stdin")]
        sources: Vec<PathBuf>,
        /// Compression level; the accepted range belongs to the codec
        /// (gzip/xz 0-9, zstd 1-22, brotli 0-11, lz4 has no level, ZIP/7z 0-9)
        #[arg(short, long, value_parser = clap::value_parser!(u32).range(0..=22))]
        level: Option<u32>,
        /// Force the archive format instead of deriving it from the output name
        #[arg(long, value_enum)]
        format: Option<PackFormat>,
        /// Split 7z output into numbered parts of at most this many bytes
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        volume_bytes: Option<u64>,
        /// Stream the archive to stdout instead of the output path; the path still selects the
        /// format, no staging or commit happens, and a failure may follow emitted bytes
        #[arg(long, conflicts_with_all = ["json", "volume_bytes"])]
        to_stdout: bool,
        /// Sort entries by member name and pin every timestamp to SOURCE_DATE_EPOCH (or 0)
        #[arg(long)]
        reproducible: bool,
        /// Compress stdin as a single-stream archive; requires a single-stream --format
        /// (gz, zst, xz, lz4, br). The output path is not written when --to-stdout is given
        #[arg(
            long,
            conflicts_with_all = [
                "volume_bytes",
                "password",
                "password_stdin",
                "reproducible",
                "seven_z_dict",
                "sources",
            ]
        )]
        from_stdin: bool,
        /// LZMA2 dictionary size in MiB for 7z output; matches the match finder's memory
        /// (about 10.5 MiB of peak RSS per MiB of dictionary). Defaults to the level's dictionary
        #[arg(long = "7z-dict", value_name = "MIB", value_parser = clap::value_parser!(u32).range(1..=1536))]
        seven_z_dict: Option<u32>,
    },

    /// Automatically compress the selection beside itself; --format picks the archive format
    #[command(name = "pack-auto")]
    PackAuto {
        /// Files or directories to compress
        #[arg(required = true)]
        sources: Vec<PathBuf>,
        /// Archive format written beside the selection (defaults to ZIP)
        #[arg(long, value_enum)]
        format: Option<PackFormat>,
    },

    /// Open the browsing window for an archive (double-click action; nothing is extracted)
    #[command(name = "open")]
    Open { archive: PathBuf },

    /// Inspect archive layout with bounded tree and display-name lengths
    #[command(name = "peek")]
    Peek {
        /// Archive path (ZIP, 7z, RAR, TAR, TAR+codec, or single stream)
        archive: PathBuf,
        /// Maximum directory tree depth to display (default: 2)
        #[arg(long, default_value_t = 2)]
        depth: usize,
        /// Maximum number of items in tree summary (default: 50)
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },

    /// Verify every member without writing extracted files
    #[command(name = "test", alias = "verify")]
    Test { archive: PathBuf },

    /// Stream a selected member to stdout without writing extracted files
    #[command(name = "cat")]
    Cat {
        /// Archive path (ZIP, 7z, RAR, TAR, TAR+codec, or single stream)
        archive: PathBuf,
        /// Relative path of the file inside the archive
        path: String,
    },

    /// Convert a ZIP archive into a self-extracting Windows executable
    #[command(name = "sfx")]
    Sfx {
        /// Output .exe path
        output: PathBuf,
        /// Input ZIP archive (password encryption is preserved)
        archive: PathBuf,
    },

    /// Add or replace files in a ZIP or 7z archive
    #[command(name = "add")]
    Add {
        archive: PathBuf,
        #[arg(required = true)]
        sources: Vec<PathBuf>,
        #[arg(short,long,value_parser=clap::value_parser!(u32).range(0..=9))]
        level: Option<u32>,
    },
    /// Delete literal members or directory subtrees from a ZIP or 7z archive
    #[command(name = "delete")]
    Delete {
        archive: PathBuf,
        #[arg(required = true)]
        members: Vec<String>,
    },
    /// Rename a member or directory subtree in a ZIP or 7z archive
    #[command(name = "rename")]
    Rename {
        archive: PathBuf,
        from: String,
        to: String,
    },

    /// Install MothZip to %LOCALAPPDATA%\Programs\MothZip and configure shell integration
    #[command(name = "install")]
    Install {
        /// Custom destination directory (defaults to %LOCALAPPDATA%\Programs\MothZip)
        #[arg(long)]
        dir: Option<PathBuf>,

        /// Do not add install directory to user PATH
        #[arg(long)]
        no_path: bool,

        /// Do not create Start Menu application shortcut
        #[arg(long)]
        no_shortcut: bool,
    },

    /// Uninstall MothZip, removing shell registrations and managed files
    #[command(name = "uninstall")]
    Uninstall {
        /// Keep install directory in user PATH
        #[arg(long)]
        keep_path: bool,
    },

    /// Show current MothZip installation, registry, and Windows integration status
    #[command(name = "status")]
    Status,

    /// Register Windows Explorer right-click context menu (portable registration)
    #[command(name = "register")]
    Register {
        /// Do not create Start Menu application shortcut
        #[arg(long)]
        no_shortcut: bool,
    },

    /// Unregister Windows Explorer right-click context menu (leaves files intact)
    #[command(name = "unregister")]
    Unregister,
}
