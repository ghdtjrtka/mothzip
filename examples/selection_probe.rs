//! Closure instrument for the selected/flat extraction body.
//!
//! Two subcommands:
//! - `make-fixtures <outdir>` writes generated archives (TAR, TGZ, GZIP, ZIP,
//!   encrypted 7z) with known content and prints a manifest mapping each member
//!   to its exact content string.
//! - `run <scenarios.json> <out_root>` runs a battery of extraction scenarios and
//!   reports the relative file tree (path -> size) produced under each target
//!   directory, leaving byte verification to the Python validator.

#[path = "../src/core/mod.rs"]
pub mod core;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use core::codec::StreamCodec;
use core::extract::{extract_archive_with_options, ExtractionOptions};
use core::format::ArchiveFormat;
use core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES;
use core::pack::{pack_archive_with_options, PackOptions};

const MAIN_RS: &[u8] = b"fn main() { println!(\"sel\"); }";
const README_MD: &[u8] = b"# selected tar fixture\n";
const HELLO_TXT: &[u8] = b"gzip selected payload";
const LEFT_X: &[u8] = b"LEFT-X-CONTENT";
const RIGHT_X: &[u8] = b"RIGHT-X-CONTENT";
const ALPHA_TXT: &[u8] = b"alpha payload";
const BETA_TXT: &[u8] = b"beta payload";
const GAMMA_TXT: &[u8] = b"gamma payload";

fn write_file(path: &Path, content: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(())
}

fn leaf_files(root: &Path) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    if !root.exists() {
        return out;
    }
    fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<String, u64>) {
        if let Ok(rd) = fs::read_dir(dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, base, out);
                } else if path.is_file() {
                    if let Ok(meta) = fs::metadata(&path) {
                        if let Ok(rel) = path.strip_prefix(base) {
                            out.insert(rel.to_string_lossy().replace('\\', "/"), meta.len());
                        }
                    }
                }
            }
        }
    }
    walk(root, root, &mut out);
    out
}

fn make_fixtures(outdir: &Path) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    fs::create_dir_all(outdir)?;
    let mut fixtures = serde_json::Map::new();

    let src = outdir.join("src");

    // TAR / TGZ: single root dir "proj".
    let proj = src.join("proj");
    write_file(&proj.join("main.rs"), MAIN_RS)?;
    write_file(&proj.join("README.md"), README_MD)?;
    let tar_archive = outdir.join("proj.tar");
    pack_archive_with_options(
        &tar_archive,
        std::slice::from_ref(&proj),
        PackOptions {
            level: None,
            password: None,
            format: Some(ArchiveFormat::Tar),
        },
    )?;
    let tgz_archive = outdir.join("proj.tgz");
    pack_archive_with_options(
        &tgz_archive,
        std::slice::from_ref(&proj),
        PackOptions {
            level: None,
            password: None,
            format: Some(ArchiveFormat::TarStream(StreamCodec::Gzip)),
        },
    )?;
    fixtures.insert(
        "tar".to_string(),
        serde_json::json!({
            "archive": tar_archive.to_string_lossy(),
            "password": null,
            "members": {
                "proj/main.rs": String::from_utf8(MAIN_RS.to_vec())?,
                "proj/README.md": String::from_utf8(README_MD.to_vec())?
            }
        }),
    );
    fixtures.insert(
        "tgz".to_string(),
        serde_json::json!({
            "archive": tgz_archive.to_string_lossy(),
            "password": null,
            "members": {
                "proj/main.rs": String::from_utf8(MAIN_RS.to_vec())?,
                "proj/README.md": String::from_utf8(README_MD.to_vec())?
            }
        }),
    );

    // GZIP: single file, member name equals the stem.
    let hello = src.join("hello.txt");
    write_file(&hello, HELLO_TXT)?;
    let gz_archive = outdir.join("hello.txt.gz");
    pack_archive_with_options(
        &gz_archive,
        std::slice::from_ref(&hello),
        PackOptions {
            level: None,
            password: None,
            format: Some(ArchiveFormat::Stream(StreamCodec::Gzip)),
        },
    )?;
    fixtures.insert(
        "gz".to_string(),
        serde_json::json!({
            "archive": gz_archive.to_string_lossy(),
            "password": null,
            "members": {
                "hello.txt": String::from_utf8(HELLO_TXT.to_vec())?
            }
        }),
    );

    // ZIP with two distinct same-leaf contents across directories.
    let left = src.join("left");
    let right = src.join("right");
    write_file(&left.join("x.txt"), LEFT_X)?;
    write_file(&right.join("x.txt"), RIGHT_X)?;
    let dup_archive = outdir.join("dup.zip");
    pack_archive_with_options(
        &dup_archive,
        &[left.clone(), right.clone()],
        PackOptions {
            level: None,
            password: None,
            format: Some(ArchiveFormat::Zip),
        },
    )?;
    fixtures.insert(
        "zip_dup".to_string(),
        serde_json::json!({
            "archive": dup_archive.to_string_lossy(),
            "password": null,
            "members": {
                "left/x.txt": String::from_utf8(LEFT_X.to_vec())?,
                "right/x.txt": String::from_utf8(RIGHT_X.to_vec())?
            }
        }),
    );

    // Encrypted 7z (AES-256, LZMA2) with a subdirectory so that selecting a
    // later member must drain earlier solid streams.
    let enc = src.join("enc");
    write_file(&enc.join("alpha.txt"), ALPHA_TXT)?;
    write_file(&enc.join("beta.txt"), BETA_TXT)?;
    write_file(&enc.join("sub").join("gamma.txt"), GAMMA_TXT)?;
    let enc7z_archive = outdir.join("enc.7z");
    pack_archive_with_options(
        &enc7z_archive,
        std::slice::from_ref(&enc),
        PackOptions {
            level: None,
            password: Some("fixture-only"),
            format: Some(ArchiveFormat::SevenZ),
        },
    )?;
    fixtures.insert(
        "enc7z".to_string(),
        serde_json::json!({
            "archive": enc7z_archive.to_string_lossy(),
            "password": "fixture-only",
            "members": {
                "enc/alpha.txt": String::from_utf8(ALPHA_TXT.to_vec())?,
                "enc/beta.txt": String::from_utf8(BETA_TXT.to_vec())?,
                "enc/sub/gamma.txt": String::from_utf8(GAMMA_TXT.to_vec())?
            }
        }),
    );

    Ok(serde_json::Value::Object(fixtures))
}

fn run_scenarios(
    scenarios_path: &Path,
    out_root: &Path,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    fs::create_dir_all(out_root)?;
    let text = fs::read_to_string(scenarios_path)?;
    let scenarios: Vec<serde_json::Value> = serde_json::from_str(&text)?;
    let mut results = Vec::new();

    for (idx, sc) in scenarios.iter().enumerate() {
        let name = sc
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(&format!("scenario_{}", idx))
            .to_string();
        let archive = sc
            .get("archive")
            .and_then(|v| v.as_str())
            .ok_or("scenario missing archive")?;
        let out_dir = PathBuf::from(
            sc.get("out_dir")
                .and_then(|v| v.as_str())
                .ok_or("scenario missing out_dir")?,
        );
        let fresh = sc.get("fresh").and_then(|v| v.as_bool()).unwrap_or(true);
        if fresh {
            let _ = fs::remove_dir_all(&out_dir);
        }
        fs::create_dir_all(&out_dir)?;

        let opts = ExtractionOptions {
            smart: sc.get("smart").and_then(|v| v.as_bool()).unwrap_or(false),
            to_folder: sc
                .get("to_folder")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            password: sc
                .get("password")
                .and_then(|v| v.as_str())
                .map(String::from),
            max_bytes: sc
                .get("max_bytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_MAX_UNCOMPRESSED_BYTES),
            members: sc
                .get("members")
                .map(|members| {
                    members
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|m| m.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .unwrap_or_default(),
            flat: sc.get("flat").and_then(|v| v.as_bool()).unwrap_or(false),
        };

        let record = match extract_archive_with_options(Path::new(archive), &out_dir, &opts) {
            Ok(summary) => {
                let files = leaf_files(&summary.target_dir);
                serde_json::json!({
                    "name": name,
                    "ok": true,
                    "error": null,
                    "summary": {
                        "extracted_files": summary.extracted_files,
                        "extracted_bytes": summary.extracted_bytes,
                        "target_dir": summary.target_dir.to_string_lossy(),
                        "smart_adjusted": summary.smart_adjusted
                    },
                    "files": files
                })
            }
            Err(e) => serde_json::json!({
                "name": name,
                "ok": false,
                "error": e.to_string(),
                "summary": null,
                "files": {}
            }),
        };
        results.push(record);
    }

    Ok(serde_json::json!({ "results": results }))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: selection_probe <make-fixtures|run> <arg> [out_root]");
        std::process::exit(2);
    }
    let result = match args[1].as_str() {
        "make-fixtures" => make_fixtures(Path::new(&args[2])),
        "run" => {
            let out_root = args
                .get(3)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("target/mercury-selected/probe-out"));
            run_scenarios(Path::new(&args[2]), &out_root)
        }
        other => Err(format!("unknown probe subcommand: {}", other).into()),
    };
    match result {
        Ok(value) => println!("{}", serde_json::to_string_pretty(&value).unwrap()),
        Err(error) => {
            eprintln!("{}", serde_json::json!({ "error": error.to_string() }));
            std::process::exit(1);
        }
    }
}
