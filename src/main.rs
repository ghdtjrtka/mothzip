use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

mod cli;
mod core;
mod shell;

use cli::args::{Cli, Commands};
use cli::output::{report_extract, report_pack, report_peek};

const SUBCOMMANDS: &[&str] = &[
    "smart-unpack",
    "unpack",
    "pack",
    "pack-auto",
    "peek",
    "cat",
    "register",
    "unregister",
    "help",
];

/// Normalize command line arguments:
/// If the user passes a file or directory directly without a subcommand
/// (e.g. `mothzip archive.zip`, or Windows Explorer drag-and-drop onto mothzip.exe),
/// automatically route to `smart-unpack` for .zip files, or `pack-auto` for directories/other files.
fn normalize_args(raw_args: Vec<String>) -> Vec<String> {
    if raw_args.len() <= 1 {
        return raw_args;
    }

    // Find the first non-flag argument after the executable path
    let mut subcmd_idx = None;
    for (i, arg) in raw_args.iter().enumerate().skip(1) {
        if !arg.starts_with('-') {
            subcmd_idx = Some(i);
            break;
        }
    }

    if let Some(idx) = subcmd_idx {
        let first_arg = &raw_args[idx];
        if !SUBCOMMANDS.contains(&first_arg.as_str()) {
            let is_zip = first_arg.ends_with(".zip")
                || first_arg.ends_with(".ZIP")
                || Path::new(first_arg)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"));

            let default_cmd = if is_zip { "smart-unpack" } else { "pack-auto" };
            let mut new_args = raw_args;
            new_args.insert(idx, default_cmd.to_string());
            return new_args;
        }
    }

    raw_args
}

/// Check whether the process was spawned directly by Windows Explorer (e.g. double-click)
/// rather than an existing interactive console shell (cmd, powershell, or agent runner).
fn is_standalone_console() -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleProcessList(lpdwProcessList: *mut u32, dwProcessCount: u32) -> u32;
    }
    let mut process_list = [0u32; 2];
    let count = unsafe { GetConsoleProcessList(process_list.as_mut_ptr(), 2) };
    count == 1
}

fn main() -> ExitCode {
    let raw_args: Vec<String> = env::args().collect();

    // If launched by double-clicking without arguments, show help
    if raw_args.len() <= 1 {
        println!("mothzip v0.1.0 - Ultra-lightweight ad-free archiver");
        println!("Run 'mothzip register' to add right-click context menu in Windows Explorer.");
        println!("Run 'mothzip --help' for full CLI commands.");
        if is_standalone_console() {
            println!("\nPress Enter to exit...");
            let mut dummy = String::new();
            let _ = io::stdin().read_line(&mut dummy);
        }
        return ExitCode::SUCCESS;
    }

    let normalized = normalize_args(raw_args);
    let cli = Cli::parse_from(normalized);

    let result = match &cli.command {
        Commands::SmartUnpack {
            archive,
            out,
            password,
            to_folder,
        } => {
            let out_dir = resolve_output_dir(archive, out);
            let mut pass = password.clone();
            let mut res = core::extract::extract_archive(
                archive,
                &out_dir,
                true,
                *to_folder,
                pass.as_deref(),
            );

            // Interactive password prompt for human terminal users
            if let Err(ref e) = res {
                if pass.is_none() && is_password_error(&e.to_string()) {
                    let arch_name = archive.file_name().and_then(|s| s.to_str()).unwrap_or("archive");
                    if let Some(prompted) = prompt_password(arch_name, cli.json) {
                        pass = Some(prompted);
                        res = core::extract::extract_archive(
                            archive,
                            &out_dir,
                            true,
                            *to_folder,
                            pass.as_deref(),
                        );
                    }
                }
            }

            match res {
                Ok(summary) => {
                    report_extract(&summary, cli.json, cli.quiet);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        Commands::Unpack {
            archive,
            out,
            password,
            to_folder,
        } => {
            let out_dir = resolve_output_dir(archive, out);
            let mut pass = password.clone();
            let mut res = core::extract::extract_archive(
                archive,
                &out_dir,
                false,
                *to_folder,
                pass.as_deref(),
            );

            if let Err(ref e) = res {
                if pass.is_none() && is_password_error(&e.to_string()) {
                    let arch_name = archive.file_name().and_then(|s| s.to_str()).unwrap_or("archive");
                    if let Some(prompted) = prompt_password(arch_name, cli.json) {
                        pass = Some(prompted);
                        res = core::extract::extract_archive(
                            archive,
                            &out_dir,
                            false,
                            *to_folder,
                            pass.as_deref(),
                        );
                    }
                }
            }

            match res {
                Ok(summary) => {
                    report_extract(&summary, cli.json, cli.quiet);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        Commands::Pack { output, sources } => match core::pack::pack_archive(output, sources) {
            Ok(summary) => {
                report_pack(&summary, cli.json, cli.quiet);
                Ok(())
            }
            Err(e) => Err(e),
        },
        Commands::PackAuto { source } => {
            let dest = core::pack::determine_auto_pack_dest(source);
            match core::pack::pack_archive(&dest, std::slice::from_ref(source)) {
                Ok(summary) => {
                    report_pack(&summary, cli.json, cli.quiet);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        },
        Commands::Peek {
            archive,
            depth,
            limit,
        } => match core::inspect::peek_archive(archive, *depth, *limit) {
            Ok(summary) => {
                report_peek(&summary, cli.json);
                Ok(())
            }
            Err(e) => Err(e),
        },
        Commands::Cat {
            archive,
            path,
            password,
        } => {
            let mut pass = password.clone();
            let mut stdout = io::stdout().lock();
            let mut res = core::inspect::cat_archive_file(
                archive,
                path,
                pass.as_deref(),
                &mut stdout,
            );

            if let Err(ref e) = res {
                if pass.is_none() && is_password_error(&e.to_string()) {
                    let arch_name = archive.file_name().and_then(|s| s.to_str()).unwrap_or("archive");
                    if let Some(prompted) = prompt_password(arch_name, cli.json) {
                        pass = Some(prompted);
                        res = core::inspect::cat_archive_file(
                            archive,
                            path,
                            pass.as_deref(),
                            &mut stdout,
                        );
                    }
                }
            }

            let _ = stdout.flush();

            // Treat broken pipe (e.g. piped to head, findstr, or stopped agent stream) as graceful exit
            if let Err(ref e) = res {
                if let Some(io_err) = e.downcast_ref::<io::Error>() {
                    if io_err.kind() == io::ErrorKind::BrokenPipe {
                        return ExitCode::SUCCESS;
                    }
                }
            }

            res
        }
        Commands::Register => shell::registry::register_context_menu(),
        Commands::Unregister => shell::registry::unregister_context_menu(),
    };

    match result {
        Ok(_) => ExitCode::SUCCESS,
        Err(err) => {
            if cli.json {
                let json_err = serde_json::json!({ "error": err.to_string() });
                eprintln!("{}", json_err);
            } else {
                eprintln!("Error: {}", err);
                if is_standalone_console() {
                    eprintln!("\nPress Enter to exit...");
                    let mut dummy = String::new();
                    let _ = io::stdin().read_line(&mut dummy);
                }
            }
            ExitCode::FAILURE
        }
    }
}

/// If output directory is not specified, default to the archive's parent directory.
fn resolve_output_dir(archive: &Path, out: &Option<PathBuf>) -> PathBuf {
    if let Some(dest) = out {
        dest.clone()
    } else if let Some(parent) = archive.parent() {
        if parent.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            parent.to_path_buf()
        }
    } else {
        PathBuf::from(".")
    }
}

fn is_password_error(msg: &str) -> bool {
    msg.contains("password-protected")
        || msg.contains("Password")
        || msg.contains("encrypted")
        || msg.contains("Decryption failed")
}

fn prompt_password(archive_name: &str, is_json: bool) -> Option<String> {
    use std::io::IsTerminal;
    if !is_json && io::stdin().is_terminal() {
        eprint!("🔑 Enter password for '{}': ", archive_name);
        let mut buf = String::new();
        if io::stdin().read_line(&mut buf).is_ok() {
            let pass = buf.trim_end_matches(['\r', '\n']).to_string();
            if !pass.is_empty() {
                return Some(pass);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_args_zip_to_smart_unpack() {
        let raw = vec!["mothzip".into(), "archive.zip".into()];
        let normalized = normalize_args(raw);
        assert_eq!(normalized, vec!["mothzip", "smart-unpack", "archive.zip"]);

        let raw_with_flags = vec!["mothzip".into(), "archive.zip".into(), "-o".into(), "out".into()];
        let normalized = normalize_args(raw_with_flags);
        assert_eq!(normalized, vec!["mothzip", "smart-unpack", "archive.zip", "-o", "out"]);
    }

    #[test]
    fn test_normalize_args_dir_to_pack_auto() {
        let raw = vec!["mothzip".into(), "my_folder".into()];
        let normalized = normalize_args(raw);
        assert_eq!(normalized, vec!["mothzip", "pack-auto", "my_folder"]);
    }

    #[test]
    fn test_normalize_args_preserves_explicit_subcommands() {
        let raw_pack = vec!["mothzip".into(), "pack".into(), "out.zip".into(), "src".into()];
        assert_eq!(normalize_args(raw_pack.clone()), raw_pack);

        let raw_peek = vec!["mothzip".into(), "peek".into(), "out.zip".into()];
        assert_eq!(normalize_args(raw_peek.clone()), raw_peek);

        let raw_help = vec!["mothzip".into(), "--help".into()];
        assert_eq!(normalize_args(raw_help.clone()), raw_help);
    }
}
