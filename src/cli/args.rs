use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "mothzip")]
#[command(author = "mothzip contributors")]
#[command(version = "0.1.0")]
#[command(
    about = "Ultra-lightweight, ad-free archiver for Windows Explorer context menu & Agentic CLI",
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
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Smart unpack: automatically detects single root folder vs loose files
    #[command(name = "smart-unpack")]
    SmartUnpack {
        /// Path to archive (.zip)
        archive: PathBuf,
        /// Destination directory (defaults to archive's parent folder)
        #[arg(short, long)]
        out: Option<PathBuf>,
    },

    /// Direct unpack: extracts directly into specified destination
    #[command(name = "unpack")]
    Unpack {
        /// Path to archive (.zip)
        archive: PathBuf,
        /// Destination directory (defaults to current directory)
        #[arg(short, long)]
        out: Option<PathBuf>,
    },

    /// Pack files or directories into a .zip archive
    #[command(name = "pack")]
    Pack {
        /// Output archive file (.zip)
        output: PathBuf,
        /// Files or directories to compress
        #[arg(required = true)]
        sources: Vec<PathBuf>,
    },

    /// Fast token-saving inspection of archive layout without extracting (Agentic Peek)
    #[command(name = "peek")]
    Peek {
        /// Path to archive (.zip)
        archive: PathBuf,
        /// Maximum directory tree depth to display (default: 2)
        #[arg(long, default_value_t = 2)]
        depth: usize,
        /// Maximum number of items in tree summary (default: 50)
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },

    /// Stream a specific file from inside the archive to stdout without decompressing
    #[command(name = "cat")]
    Cat {
        /// Path to archive (.zip)
        archive: PathBuf,
        /// Relative path of the file inside the archive
        path: String,
    },

    /// Register Windows Explorer right-click context menu (Smart Unpack, Pack)
    #[command(name = "register")]
    Register,

    /// Unregister Windows Explorer right-click context menu
    #[command(name = "unregister")]
    Unregister,
}
