use clap::Parser;
use std::env;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
mod cli;
mod core;
mod shell;
mod ui;
use cli::args::{Cli, Commands};
use cli::output::{report_extract, report_pack, report_peek};
type AppResult<T> = Result<T, Box<dyn std::error::Error>>;
const SUBCOMMANDS: &[&str] = &[
    "smart-unpack",
    "unpack",
    "pack",
    "pack-auto",
    "open",
    "peek",
    "cat",
    "sfx",
    "add",
    "delete",
    "rename",
    "test",
    "verify",
    "install",
    "uninstall",
    "status",
    "register",
    "unregister",
    "help",
];

/// Normalize command line arguments:
/// If the user passes a file or directory directly without a subcommand
/// (e.g. `mothzip archive.zip`, or Windows Explorer drag-and-drop onto mothzip.exe),
/// automatically route to `smart-unpack` for .zip files, or `pack-auto` for directories/other files.
fn normalize_args(mut raw_args: Vec<String>) -> Vec<String> {
    let mut i = 1;
    while i < raw_args.len() {
        let arg = &raw_args[i];
        if arg == "--password"
            || arg == "--max-bytes"
            || (arg.starts_with('-') && !arg.starts_with("--") && arg.ends_with('p'))
        {
            i += 2;
            continue;
        }
        if arg.starts_with('-') {
            i += 1;
            continue;
        }
        if !SUBCOMMANDS.contains(&arg.as_str()) {
            let command = if core::format::is_supported_archive(arg) {
                "smart-unpack"
            } else {
                "pack-auto"
            };
            raw_args.insert(i, command.to_string());
        }
        break;
    }
    raw_args
}

fn standalone_console() -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
    }
    let mut list = [0u32; 2];
    unsafe { GetConsoleProcessList(list.as_mut_ptr(), 2) == 1 }
}

fn main() -> ExitCode {
    let mut raw: Vec<String> = env::args().collect();
    // Embedded payloads use the ordinary extraction command and its CLI contract.
    let own_archive = match env::current_exe()
        .and_then(|path| core::sfx::embedded_archive(&path).map(|info| info.map(|_| path)))
    {
        Ok(value) => value,
        Err(error) => {
            if raw.iter().any(|arg| arg == "--json") {
                eprintln!("{}", serde_json::json!({"error":error.to_string()}));
            } else {
                eprintln!("Error: {error}");
            }
            return ExitCode::FAILURE;
        }
    };
    if let Some(archive) = own_archive {
        let explicit = raw.get(1).is_some_and(|arg| {
            SUBCOMMANDS.contains(&arg.as_str())
                || ["--help", "-h", "--version", "-V"].contains(&arg.as_str())
        });
        if !explicit {
            let desktop = raw.len() == 1 && io::stdin().is_terminal() && standalone_console();
            raw.insert(1, "smart-unpack".into());
            raw.insert(2, archive.to_string_lossy().into_owned());
            if desktop {
                raw.push("--open".into());
            }
        }
    }
    if raw.len() == 1 && io::stdin().is_terminal() && standalone_console() {
        let mut filters = core::format::supported_extensions()
            .iter()
            .map(|ext| ext.trim_start_matches('.'))
            .collect::<Vec<_>>();
        filters.push("001");
        let selected = rfd::FileDialog::new()
            .set_title("나방집 — 압축 파일 선택")
            .add_filter("Archives", &filters)
            .add_filter("All files", &["*"])
            .pick_file();
        let Some(file) = selected else {
            return ExitCode::SUCCESS;
        };
        raw.extend([
            "smart-unpack".into(),
            file.to_string_lossy().into_owned(),
            "--open".into(),
        ]);
    }
    let normalized = normalize_args(raw);
    let json_requested = normalized.iter().any(|arg| arg == "--json");
    let cli = match Cli::try_parse_from(normalized) {
        Ok(cli) => cli,
        Err(error) => {
            if json_requested && error.use_stderr() {
                eprintln!("{}", serde_json::json!({"error": error.to_string()}));
            } else {
                let _ = error.print();
            }
            return ExitCode::from(error.exit_code() as u8);
        }
    };
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if cli.json {
                eprintln!("{}", serde_json::json!({"error":error.to_string()}));
            } else {
                eprintln!("Error: {}", error);
                if matches!(
                    cli.command,
                    Commands::SmartUnpack { open: true, .. } | Commands::Unpack { open: true, .. }
                ) && !cli.non_interactive
                {
                    rfd::MessageDialog::new()
                        .set_title("MothZip")
                        .set_description(error.to_string())
                        .set_level(rfd::MessageLevel::Error)
                        .show();
                }
            }
            ExitCode::FAILURE
        }
    }
}

/// Archive path `pack-auto` writes: beside the selection, named after it. Several selected items
/// become one archive named after their containing folder, like Explorer's own "Compress to ZIP".
fn auto_pack_dest(sources: &[PathBuf], suffix: &str) -> PathBuf {
    let Some(first) = sources.first() else {
        return PathBuf::from(format!("archive.{suffix}"));
    };
    if sources.len() == 1 {
        let mut destination = core::pack::determine_auto_pack_dest(first);
        destination.set_extension(suffix);
        if destination == *first {
            let stem = first
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("archive");
            destination = first
                .parent()
                .unwrap_or(Path::new("."))
                .join(format!("{stem}_packed.{suffix}"));
        }
        return destination;
    }
    let parent = first.parent().unwrap_or(Path::new("."));
    let name = parent
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("archive");
    parent.join(format!("{name}.{suffix}"))
}

fn run(cli: &Cli) -> AppResult<()> {
    let archive_command = matches!(
        cli.command,
        Commands::SmartUnpack { .. }
            | Commands::Unpack { .. }
            | Commands::Pack { .. }
            | Commands::PackAuto { .. }
            | Commands::Peek { .. }
            | Commands::Cat { .. }
            | Commands::Test { .. }
            | Commands::Sfx { .. }
            | Commands::Open { .. }
            | Commands::Add { .. }
            | Commands::Delete { .. }
            | Commands::Rename { .. }
    );
    if !archive_command && (cli.password.is_some() || cli.password_stdin) {
        return Err("Password options apply only to archive commands".into());
    }
    let password = if cli.password_stdin {
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            return Err("Password stdin is empty".into());
        }
        Some(line.trim_end_matches(['\r', '\n']).to_string())
    } else {
        cli.password.clone()
    };
    match &cli.command {
        Commands::SmartUnpack {
            archive,
            out,
            to_folder,
            members,
            flat,
            open,
        }
        | Commands::Unpack {
            archive,
            out,
            to_folder,
            members,
            flat,
            open,
        } => {
            let smart = matches!(cli.command, Commands::SmartUnpack { .. });
            let destination = resolve_output_dir(archive, out);
            if *open && !cli.quiet {
                eprintln!("Extracting {}...", archive.display());
            }
            let result = with_password(cli, archive, &password, |pw| {
                if members.is_empty() && !*flat {
                    return core::extract::extract_archive(
                        archive,
                        &destination,
                        smart,
                        *to_folder,
                        pw,
                        cli.max_bytes,
                    );
                }
                core::extract::extract_archive_with_options(
                    archive,
                    &destination,
                    &core::extract::ExtractionOptions {
                        smart,
                        to_folder: *to_folder,
                        password: pw.map(str::to_owned),
                        max_bytes: cli.max_bytes,
                        members: members.clone(),
                        flat: *flat,
                    },
                )
            })?;
            report_extract(&result, cli.json, cli.quiet);
            if *open {
                std::process::Command::new(shell::system::get_explorer_exe()?)
                    .arg(&result.target_dir)
                    .spawn()?;
            }
        }
        Commands::Pack {
            output,
            sources,
            level,
            format,
            volume_bytes,
            to_stdout,
            reproducible,
            from_stdin,
            seven_z_dict,
        } => {
            let format = format.map(|value| value.archive_format());
            if *from_stdin {
                // A pipe carries bytes, not entry names, so only a single-stream codec can be
                // written from stdin; anything else would have to invent member metadata.
                let format = match format {
                    Some(format) => format,
                    None => core::pack::resolve_pack_format(output, None)?,
                };
                let codec = match format {
                    core::format::ArchiveFormat::Stream(codec) => codec,
                    other => {
                        return Err(format!(
                            "--from-stdin writes a single compressed stream; {} needs entry \
                             names, so pack it from files instead",
                            other.name()
                        )
                        .into())
                    }
                };
                let level = *level;
                if *to_stdout {
                    match core::pack::pack_stream_from_reader(
                        io::stdin().lock(),
                        io::stdout().lock(),
                        codec,
                        level,
                    ) {
                        Err(ref error) if is_broken_pipe(error.as_ref()) => return Ok(()),
                        other => other?,
                    }
                    if let Err(error) = io::stdout().flush() {
                        if error.kind() != io::ErrorKind::BrokenPipe {
                            return Err(error.into());
                        }
                    }
                    return Ok(());
                }
                let file = std::fs::File::create(output)?;
                core::pack::pack_stream_from_reader(io::stdin().lock(), file, codec, level)?;
                let bytes = std::fs::metadata(output)?.len();
                if cli.json {
                    report(
                        &serde_json::json!({
                            "archive_path": output,
                            "format": format.name(),
                            "compressed_bytes": bytes,
                            "source": "stdin"
                        }),
                        cli,
                    )?;
                } else if !cli.quiet {
                    println!("{} ({} bytes from stdin)", output.display(), bytes);
                }
                return Ok(());
            }
            if *to_stdout {
                // The archive owns stdout from here: no summary, no JSON, no second writer. The
                // path still selects the format, and 7z is refused before the first byte because
                // it seeks back to its signature header when it finishes.
                let format = core::pack::resolve_pack_format(output, format)?;
                let request = core::pack::PackRequest {
                    options: core::pack::PackOptions {
                        level: *level,
                        password: password.as_deref(),
                        format: Some(format),
                    },
                    reproducible: *reproducible,
                    seven_z_dictionary_bytes: seven_z_dict.map(|mib| mib * 1024 * 1024),
                };
                match core::pack::pack_archive_to_sink(io::stdout().lock(), sources, request) {
                    Err(ref error) if is_broken_pipe(error.as_ref()) => return Ok(()),
                    other => other?,
                }
                if let Err(error) = io::stdout().flush() {
                    if error.kind() != io::ErrorKind::BrokenPipe {
                        return Err(error.into());
                    }
                }
                return Ok(());
            }
            let request = core::pack::PackRequest {
                options: core::pack::PackOptions {
                    level: *level,
                    password: password.as_deref(),
                    format,
                },
                reproducible: *reproducible,
                seven_z_dictionary_bytes: seven_z_dict.map(|mib| mib * 1024 * 1024),
            };
            let result = core::pack::pack_archive_request(output, sources, request, *volume_bytes)?;
            report_pack(&result, cli.json, cli.quiet);
        }
        Commands::PackAuto { sources, format } => {
            let format = format.unwrap_or(crate::cli::args::PackFormat::Zip);
            let destination = auto_pack_dest(sources, format.suffix());
            let result = core::pack::pack_archive_with_options(
                &destination,
                sources,
                core::pack::PackOptions {
                    password: password.as_deref(),
                    format: Some(format.archive_format()),
                    ..Default::default()
                },
            )?;
            report_pack(&result, cli.json, cli.quiet);
        }
        Commands::Open { archive } => {
            if cli.non_interactive {
                return Err(
                    "The browsing window is unavailable with --non-interactive; use peek, cat, or unpack"
                        .into(),
                );
            }
            ui::open_archive_window(&ui::BrowseRequest {
                archive: archive.clone(),
                password: password.clone(),
                max_bytes: cli.max_bytes,
            })?;
        }
        Commands::Peek {
            archive,
            depth,
            limit,
        } => {
            let result = with_password(cli, archive, &password, |pw| {
                core::inspect::peek_archive_with_password(
                    archive,
                    *depth,
                    *limit,
                    pw,
                    cli.max_bytes,
                )
            })?;
            if cli.json || !cli.quiet {
                report_peek(&result, cli.json);
            }
        }
        Commands::Test { archive } => {
            let result =
                core::inspect::test_archive_integrity(archive, password.as_deref(), cli.max_bytes)?;
            report(&result, cli)?;
            if !result.passed {
                return Err(result
                    .error_message
                    .unwrap_or_else(|| "Integrity check failed".into())
                    .into());
            }
        }
        Commands::Cat { archive, path } => {
            // Raw output cannot safely be retried after partial bytes have been emitted.
            let mut stdout = io::stdout().lock();
            let result = core::inspect::cat_archive_file(
                archive,
                path,
                password.as_deref(),
                &mut stdout,
                cli.max_bytes,
            );
            match result {
                Err(ref error) if is_broken_pipe(error.as_ref()) => return Ok(()),
                other => other?,
            }
            stdout.flush()?;
        }
        Commands::Sfx { output, archive } => {
            let result = core::sfx::create_self_extractor(
                output,
                archive,
                password.as_deref(),
                cli.max_bytes,
            )?;
            report(&result, cli)?;
        }
        Commands::Add { archive, .. }
        | Commands::Delete { archive, .. }
        | Commands::Rename { archive, .. } => {
            let (operation, level) = match &cli.command {
                Commands::Add { sources, level, .. } => (
                    core::edit::EditOp::Add {
                        sources: sources.clone(),
                    },
                    *level,
                ),
                Commands::Delete { members, .. } => (
                    core::edit::EditOp::Delete {
                        members: members.clone(),
                    },
                    None,
                ),
                Commands::Rename { from, to, .. } => (
                    core::edit::EditOp::Rename {
                        from: from.clone(),
                        to: to.clone(),
                    },
                    None,
                ),
                _ => unreachable!("archive edit command matched"),
            };
            let result = with_password(cli, archive, &password, |pw| {
                core::edit::edit_archive(
                    archive,
                    std::slice::from_ref(&operation),
                    &core::edit::EditOptions {
                        password: pw,
                        level,
                        max_bytes: cli.max_bytes,
                    },
                )
            })?;
            report(&result, cli)?;
        }
        Commands::Install {
            dir,
            no_path,
            no_shortcut,
        } => report(
            &shell::install(&shell::InstallOptions {
                dir: dir.clone(),
                no_path: *no_path,
                no_shortcut: *no_shortcut,
            })?,
            cli,
        )?,
        Commands::Uninstall { keep_path } => report(
            &shell::uninstall(&shell::UninstallOptions {
                keep_path: *keep_path,
            })?,
            cli,
        )?,
        Commands::Status => report(&shell::get_status()?, cli)?,
        Commands::Register { no_shortcut } => report(
            &shell::register(&shell::RegisterOptions {
                no_shortcut: *no_shortcut,
            })?,
            cli,
        )?,
        Commands::Unregister => report(&shell::unregister()?, cli)?,
    }
    Ok(())
}

fn report<T: serde::Serialize>(value: &T, cli: &Cli) -> AppResult<()> {
    if cli.json || !cli.quiet {
        println!("{}", serde_json::to_string_pretty(value)?);
    }
    Ok(())
}

/// A consumer that stopped reading is how a pipeline ends (`| head -c 100`), and it has already
/// taken the bytes it asked for, so every raw-stdout producer treats it as success. The sink
/// failure can sit under a codec or ZIP error, so the whole source chain is inspected.
fn is_broken_pipe(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(io_error) = error.downcast_ref::<io::Error>() {
            if io_error.kind() == io::ErrorKind::BrokenPipe {
                return true;
            }
        }
        current = error.source();
    }
    false
}

fn with_password<T>(
    cli: &Cli,
    path: &Path,
    password: &Option<String>,
    mut operation: impl FnMut(Option<&str>) -> AppResult<T>,
) -> AppResult<T> {
    match operation(password.as_deref()) {
        Err(error)
            if password.is_none()
                && !cli.password_stdin
                && !cli.json
                && !cli.non_interactive
                && io::stdin().is_terminal()
                && error.to_string().to_ascii_lowercase().contains("password") =>
        {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("archive");
            let prompted = cli::password::read_password(name)?;
            operation(Some(&prompted))
        }
        result => result,
    }
}

fn resolve_output_dir(archive: &Path, out: &Option<PathBuf>) -> PathBuf {
    out.clone().unwrap_or_else(|| {
        archive
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_args_zip_to_smart_unpack() {
        let raw = vec!["mothzip".into(), "archive.zip".into()];
        let normalized = normalize_args(raw);
        assert_eq!(normalized, vec!["mothzip", "smart-unpack", "archive.zip"]);

        let raw_with_flags = vec![
            "mothzip".into(),
            "archive.zip".into(),
            "-o".into(),
            "out".into(),
        ];
        let normalized = normalize_args(raw_with_flags);
        assert_eq!(
            normalized,
            vec!["mothzip", "smart-unpack", "archive.zip", "-o", "out"]
        );
    }

    #[test]
    fn test_normalize_args_dir_to_pack_auto() {
        let raw = vec!["mothzip".into(), "my_folder".into()];
        let normalized = normalize_args(raw);
        assert_eq!(normalized, vec!["mothzip", "pack-auto", "my_folder"]);
    }

    #[test]
    fn test_normalize_args_preserves_explicit_subcommands() {
        let raw_pack = vec![
            "mothzip".into(),
            "pack".into(),
            "out.zip".into(),
            "src".into(),
        ];
        assert_eq!(normalize_args(raw_pack.clone()), raw_pack);

        let raw_peek = vec!["mothzip".into(), "peek".into(), "out.zip".into()];
        assert_eq!(normalize_args(raw_peek.clone()), raw_peek);

        let raw_help = vec!["mothzip".into(), "--help".into()];
        assert_eq!(normalize_args(raw_help.clone()), raw_help);

        let raw_install = vec!["mothzip".into(), "install".into()];
        assert_eq!(normalize_args(raw_install.clone()), raw_install);

        let raw_status = vec!["mothzip".into(), "status".into(), "--json".into()];
        assert_eq!(normalize_args(raw_status.clone()), raw_status);

        let raw_uninstall = vec!["mothzip".into(), "uninstall".into(), "--quiet".into()];
        assert_eq!(normalize_args(raw_uninstall.clone()), raw_uninstall);
    }
}
