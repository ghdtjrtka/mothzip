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

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();

    // If launched by double-clicking without arguments, show help
    if args.len() <= 1 {
        println!("mothzip v0.1.0 - Ultra-lightweight ad-free archiver");
        println!("Run 'mothzip register' to add right-click context menu in Windows Explorer.");
        println!("Run 'mothzip --help' for full CLI commands.");
        return ExitCode::SUCCESS;
    }

    let cli = Cli::parse();

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
