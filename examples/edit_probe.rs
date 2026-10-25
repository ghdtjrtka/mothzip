//! Probe uses the real archive editor and transaction.
#![allow(dead_code)]
#[path = "../src/core/mod.rs"]
mod core;
use core::edit;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES;
use edit::{edit_archive, EditOp, EditOptions};

const ALPHA: &[u8] = b"alpha content";
const BETA: &[u8] = b"beta content";
const GAMMA: &[u8] = b"gamma content";
const NEWTXT: &[u8] = b"newly added content";
const XDATA: &[u8] = b"data/x content";

fn write_file(path: &Path, content: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(())
}

fn write_zip_archive(dest: &Path, encrypted: bool) -> Result<(), Box<dyn std::error::Error>> {
    let file = fs::File::create(dest)?;
    let mut writer = zip::ZipWriter::new(file);
    let mut base = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    if encrypted {
        base = base.with_aes_encryption(zip::AesMode::Aes256, "fixture-only");
    }
    writer.start_file("a.txt", base)?;
    writer.write_all(ALPHA)?;
    writer.add_directory("dir/", base)?;
    writer.start_file("dir/b.txt", base)?;
    writer.write_all(BETA)?;
    writer.add_directory("dir/sub/", base)?;
    writer.start_file("dir/sub/c.txt", base)?;
    writer.write_all(GAMMA)?;
    writer.finish()?.sync_all()?;
    Ok(())
}

fn write_7z_archive(dest: &Path, encrypted: bool) -> Result<(), Box<dyn std::error::Error>> {
    let file = fs::File::create(dest)?;
    let mut writer = sevenz_rust2::ArchiveWriter::new(file)
        .map_err(|e| format!("7z writer create failed: {e}"))?;
    let mut methods = Vec::new();
    if encrypted {
        methods.push(sevenz_rust2::EncoderConfiguration::from(
            sevenz_rust2::encoder_options::AesEncoderOptions::new(sevenz_rust2::Password::from(
                "fixture-only",
            )),
        ));
    }
    methods.push(
        sevenz_rust2::EncoderConfiguration::new(sevenz_rust2::EncoderMethod::LZMA2).with_options(
            sevenz_rust2::encoder_options::EncoderOptions::Lzma2(
                sevenz_rust2::encoder_options::Lzma2Options::from_level(5),
            ),
        ),
    );
    writer.set_content_methods(methods);

    let mut push =
        |name: &str, data: &[u8], is_dir: bool| -> Result<(), Box<dyn std::error::Error>> {
            let entry = sevenz_rust2::ArchiveEntry {
                name: name.to_string(),
                has_stream: !is_dir,
                is_directory: is_dir,
                ..Default::default()
            };
            if is_dir {
                writer.push_archive_entry(entry, None::<&[u8]>)?;
            } else {
                writer.push_archive_entry(entry, Some(data))?;
            }
            Ok(())
        };

    push("a.txt", ALPHA, false)?;
    push("dir", &[], true)?;
    push("dir/b.txt", BETA, false)?;
    push("dir/sub", &[], true)?;
    push("dir/sub/c.txt", GAMMA, false)?;

    writer
        .finish()
        .map_err(|e| format!("7z finalize failed: {e}"))?
        .sync_all()?;
    Ok(())
}

fn make_fixtures(outdir: &Path) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    fs::create_dir_all(outdir)?;

    let add = outdir.join("add");
    write_file(&add.join("new.txt"), NEWTXT)?;
    write_file(&add.join("data").join("x.txt"), XDATA)?;

    let zip_archive = outdir.join("base.zip");
    write_zip_archive(&zip_archive, false)?;

    let enc_zip_archive = outdir.join("enc.zip");
    write_zip_archive(&enc_zip_archive, true)?;

    let sz_archive = outdir.join("base.7z");
    write_7z_archive(&sz_archive, false)?;

    let enc_archive = outdir.join("enc.7z");
    write_7z_archive(&enc_archive, true)?;

    let members = serde_json::json!({
        "a.txt": String::from_utf8(ALPHA.to_vec())?,
        "dir/b.txt": String::from_utf8(BETA.to_vec())?,
        "dir/sub/c.txt": String::from_utf8(GAMMA.to_vec())?
    });

    Ok(serde_json::json!({
        "members": members,
        "add": {
            "new.txt": String::from_utf8(NEWTXT.to_vec())?,
            "data/x.txt": String::from_utf8(XDATA.to_vec())?
        },
        "zip": zip_archive.to_string_lossy(),
        "enczip": enc_zip_archive.to_string_lossy(),
        "sz7z": sz_archive.to_string_lossy(),
        "enc7z": enc_archive.to_string_lossy(),
        "enc_password": "fixture-only"
    }))
}

fn parse_password(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--password" {
            return args.get(i + 1).cloned();
        }
        i += 1;
    }
    None
}

fn run_edit(
    archive: &Path,
    ops_path: &Path,
    out_json: &Path,
    password: Option<String>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let text = fs::read_to_string(ops_path)?;
    let ops_value: serde_json::Value = serde_json::from_str(&text)?;

    let mut ops: Vec<EditOp> = Vec::new();
    if let Some(add) = ops_value.get("add").and_then(|v| v.as_array()) {
        let sources: Vec<PathBuf> = add
            .iter()
            .filter_map(|v| v.as_str())
            .map(PathBuf::from)
            .collect();
        ops.push(EditOp::Add { sources });
    }
    if let Some(del) = ops_value.get("delete").and_then(|v| v.as_array()) {
        let members: Vec<String> = del
            .iter()
            .filter_map(|v| v.as_str())
            .map(String::from)
            .collect();
        ops.push(EditOp::Delete { members });
    }
    if let Some(ren) = ops_value.get("rename").and_then(|v| v.as_array()) {
        if ren.len() == 2 {
            let from = ren[0].as_str().unwrap_or("").to_string();
            let to = ren[1].as_str().unwrap_or("").to_string();
            ops.push(EditOp::Rename { from, to });
        }
    }

    let opts = EditOptions {
        password: password.as_deref(),
        level: ops_value
            .get("level")
            .and_then(|v| v.as_u64())
            .map(|l| l as u32),
        max_bytes: ops_value
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    };

    let value = match edit_archive(archive, &ops, &opts) {
        Ok(summary) => serde_json::json!({ "ok": true, "error": null, "summary": summary }),
        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string(), "summary": null }),
    };
    fs::write(out_json, serde_json::to_string_pretty(&value)?)?;
    Ok(value)
}

fn run_catalog(
    archive: &Path,
    password: Option<String>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let entries = core::catalog::read_archive_catalog(
        archive,
        password.as_deref(),
        DEFAULT_MAX_UNCOMPRESSED_BYTES,
    )?;
    Ok(serde_json::to_value(entries)?)
}

fn run_digest(
    archive: &Path,
    member: &str,
    password: Option<String>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let mut hashed = FnvWriter::default();
    core::inspect::cat_archive_file(
        archive,
        member,
        password.as_deref(),
        &mut hashed,
        DEFAULT_MAX_UNCOMPRESSED_BYTES,
    )?;
    Ok(serde_json::json!({
        "len": hashed.len as u64,
        "fnv": format!("{:016x}", hashed.hash)
    }))
}

#[derive(Default)]
struct FnvWriter {
    hash: u64,
    len: usize,
}

impl Write for FnvWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut h = if self.len == 0 {
            0xcbf2_9ce4_8422_2325u64
        } else {
            self.hash
        };
        for b in buf {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.hash = h;
        self.len += buf.len();
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: edit_probe <make-fixtures|edit|catalog|digest> ...");
        std::process::exit(2);
    }
    let result: Result<serde_json::Value, Box<dyn std::error::Error>> = match args[1].as_str() {
        "make-fixtures" => make_fixtures(Path::new(&args[2])),
        "edit" => {
            if args.len() < 5 {
                Err("edit requires <archive> <ops.json> <out.json> [--password P]".into())
            } else {
                run_edit(
                    Path::new(&args[2]),
                    Path::new(&args[3]),
                    Path::new(&args[4]),
                    parse_password(&args),
                )
            }
        }
        "catalog" => run_catalog(Path::new(&args[2]), parse_password(&args)),
        "digest" => {
            if args.len() < 4 {
                Err("digest requires <archive> <member> [--password P]".into())
            } else {
                run_digest(Path::new(&args[2]), &args[3], parse_password(&args))
            }
        }
        other => Err(format!("unknown probe subcommand: {other}").into()),
    };

    match result {
        Ok(value) => println!("{}", serde_json::to_string(&value).unwrap()),
        Err(e) => {
            eprintln!("{}", serde_json::json!({ "error": e.to_string() }));
            std::process::exit(1);
        }
    }
}
