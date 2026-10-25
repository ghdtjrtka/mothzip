//! Split-archive creation probe. Exercises the owned split body without touching main.rs/CLI.
//! Build with:  CARGO_TARGET_DIR=target/mercury-split cargo build --example split_probe
//! Run with:    target/mercury-split/debug/examples/split_probe.exe [scratch_root]
#![allow(clippy::needless_range_loop)]

#[path = "../src/core/mod.rs"]
pub mod core;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES;
use core::pack::PackOptions;

fn hash_bytes(data: &[u8]) -> String {
    let mut h = [0u8; 32];
    let mut state: u64 = 0xcbf29ce484222325;
    for &b in data {
        state ^= b as u64;
        state = state.wrapping_mul(0x100000001b3);
    }
    for i in 0..32 {
        h[i] = ((state >> ((i % 8) * 8)) & 0xff) as u8;
    }
    h.iter().map(|b| format!("{b:02x}")).collect()
}

fn hash_file(p: &Path) -> String {
    hash_bytes(&fs::read(p).unwrap_or_default())
}

fn lcg(seed: &mut u64) -> u8 {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    ((*seed >> 33) & 0xff) as u8
}

fn fill_deterministic(path: &Path, size: usize, seed: &mut u64) {
    let mut data = vec![0u8; size];
    for b in data.iter_mut() {
        *b = lcg(seed);
    }
    fs::write(path, data).unwrap();
}

/// Recursively collect relative path -> sha256 for all regular files under root.
fn manifest(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for ent in fs::read_dir(&dir).unwrap() {
            let ent = ent.unwrap();
            let p = ent.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.is_file() {
                let rel = p
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, hash_file(&p));
            }
        }
    }
    out
}

fn extract_all(archive: &Path, out: &Path, password: Option<&str>) -> Result<(), String> {
    core::extract::extract_archive(
        archive,
        out,
        false,
        false,
        password,
        DEFAULT_MAX_UNCOMPRESSED_BYTES,
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// A directory source is archived with its folder name as a leading path component, so the
/// expected extraction manifest must be keyed relative to the archive root.
fn archived_expect(src: &Path, expect: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let prefix = src
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    expect
        .iter()
        .map(|(k, v)| (format!("{prefix}/{k}"), v.clone()))
        .collect()
}

struct Report {
    cases: Vec<(String, bool, String)>,
}
impl Report {
    fn new() -> Self {
        Report { cases: Vec::new() }
    }
    fn add(&mut self, name: &str, pass: bool, detail: String) {
        self.cases.push((name.to_string(), pass, detail));
    }
    fn emit(&self) {
        let arr: Vec<serde_json::Value> = self
            .cases
            .iter()
            .map(|(name, pass, detail)| {
                serde_json::json!({"case": name, "pass": pass, "detail": detail})
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap());
    }
}

fn scratch_root(arg: Option<&str>) -> PathBuf {
    match arg {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir()
            .unwrap()
            .join("target/mercury-split/probe"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = scratch_root(args.get(1).map(|s| s.as_str()));
    fs::create_dir_all(&root).unwrap();
    let mut rep = Report::new();

    case_split_roundtrip(&root, &mut rep);
    case_plain_roundtrip(&root, &mut rep);
    case_encrypted_split_roundtrip(&root, &mut rep);
    case_size_constraint(&root, &mut rep);
    case_empty_archive(&root, &mut rep);
    case_long_unicode(&root, &mut rep);
    case_replace_many_to_fewer(&root, &mut rep);
    case_stale_tail_rollback(&root, &mut rep);
    case_directory_tail_refusal(&root, &mut rep);
    case_reject_non_7z_split(&root, &mut rep);
    case_reject_zero_volume(&root, &mut rep);

    rep.emit();
}

fn setup_sources(
    root: &Path,
    name: &str,
    files: &[(String, usize)],
    seed: u64,
) -> (PathBuf, BTreeMap<String, String>) {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    let mut s = seed;
    for (rel, size) in files {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fill_deterministic(&p, *size, &mut s);
    }
    let m = manifest(&dir);
    (dir, m)
}

fn case_split_roundtrip(root: &Path, rep: &mut Report) {
    let (src, expect) = setup_sources(
        root,
        "split_rt",
        &[
            ("a.txt".to_string(), 4000),
            ("sub/b.bin".to_string(), 120000),
            ("sub/deep/c.log".to_string(), 50),
        ],
        101,
    );
    let dest = root.join("split_rt/arch.7z");
    let opts = PackOptions::default();
    let res =
        core::pack::pack_archive_split_with_options(&dest, std::slice::from_ref(&src), opts, 2048);
    match res {
        Ok(s) => {
            let vol_ok = !s.volumes.is_empty()
                && s.volume_sizes.iter().all(|&sz| sz <= 2048)
                && s.volumes.iter().all(|p| p.exists());
            let out = root.join("split_rt/out");
            let ext = extract_all(&s.archive_path, &out, None);
            let got = manifest(&out);
            let expect_archived = archived_expect(&src, &expect);
            let match_ok = match &ext {
                Ok(()) => got == expect_archived,
                Err(_) => false,
            };
            rep.add(
                "split_roundtrip",
                vol_ok && match_ok,
                format!(
                    "volumes={} sizes={:?} extract_ok={} bytes_match={}",
                    s.volumes.len(),
                    s.volume_sizes,
                    ext.is_ok(),
                    match_ok
                ),
            );
        }
        Err(e) => rep.add("split_roundtrip", false, format!("pack error: {e}")),
    }
}

fn case_plain_roundtrip(root: &Path, rep: &mut Report) {
    let (src, expect) = setup_sources(
        root,
        "plain_rt",
        &[("a.txt".to_string(), 3000), ("b.bin".to_string(), 90000)],
        202,
    );
    let dest = root.join("plain_rt/arch.7z");
    let opts = PackOptions::default();
    let res = core::pack::pack_archive_with_options(&dest, std::slice::from_ref(&src), opts);
    match res {
        Ok(s) => {
            let no_vol = s.volumes.is_empty() && s.volume_sizes.is_empty();
            let out = root.join("plain_rt/out");
            let ext = extract_all(&dest, &out, None);
            let got = manifest(&out);
            let expect_archived = archived_expect(&src, &expect);
            let match_ok = ext.is_ok() && got == expect_archived;
            rep.add(
                "plain_roundtrip",
                no_vol && match_ok,
                format!("extract_ok={} bytes_match={}", ext.is_ok(), match_ok),
            );
        }
        Err(e) => rep.add("plain_roundtrip", false, format!("pack error: {e}")),
    }
}

fn case_encrypted_split_roundtrip(root: &Path, rep: &mut Report) {
    let (src, expect) = setup_sources(
        root,
        "enc_split_rt",
        &[
            ("secret.txt".to_string(), 8000),
            ("blob.bin".to_string(), 60000),
        ],
        303,
    );
    let dest = root.join("enc_split_rt/arch.7z");
    let opts = PackOptions {
        password: Some("probe-pass"),
        ..PackOptions::default()
    };
    let res =
        core::pack::pack_archive_split_with_options(&dest, std::slice::from_ref(&src), opts, 4096);
    match res {
        Ok(s) => {
            let out = root.join("enc_split_rt/out");
            let ext_ok = extract_all(&s.archive_path, &out, Some("probe-pass")).is_ok();
            let got = manifest(&out);
            let expect_archived = archived_expect(&src, &expect);
            let match_ok = ext_ok && got == expect_archived;
            rep.add(
                "encrypted_split_roundtrip",
                match_ok,
                format!("extract_ok={} bytes_match={}", ext_ok, match_ok),
            );
        }
        Err(e) => rep.add(
            "encrypted_split_roundtrip",
            false,
            format!("pack error: {e}"),
        ),
    }
}

fn case_size_constraint(root: &Path, rep: &mut Report) {
    let (src, _) = setup_sources(
        root,
        "size_limit",
        &[
            ("big.bin".to_string(), 200000),
            ("small.txt".to_string(), 5000),
        ],
        404,
    );
    let dest = root.join("size_limit/arch.7z");
    let opts = PackOptions::default();
    let res =
        core::pack::pack_archive_split_with_options(&dest, std::slice::from_ref(&src), opts, 1024);
    match res {
        Ok(s) => {
            let multi = s.volumes.len() >= 2;
            let all_le = s.volume_sizes.iter().all(|&sz| sz <= 1024);
            let last_short_ok = s.volume_sizes.last().map(|&sz| sz <= 1024).unwrap_or(false);
            let joined_len: u64 = s.volume_sizes.iter().sum();
            let joined_ok = joined_len > 0;
            rep.add(
                "size_constraint",
                multi && all_le && last_short_ok && joined_ok,
                format!(
                    "count={} sizes={:?} all_le={} last_le={}",
                    s.volumes.len(),
                    s.volume_sizes,
                    all_le,
                    last_short_ok
                ),
            );
        }
        Err(e) => rep.add("size_constraint", false, format!("pack error: {e}")),
    }
}

fn case_empty_archive(root: &Path, rep: &mut Report) {
    let dir = root.join("empty_src");
    fs::create_dir_all(&dir).unwrap();
    let dest = root.join("empty_src/arch.7z");
    let opts = PackOptions::default();
    let res =
        core::pack::pack_archive_split_with_options(&dest, std::slice::from_ref(&dir), opts, 2048);
    match res {
        Ok(s) => {
            let out = root.join("empty_src/out");
            let ext_ok = extract_all(&s.archive_path, &out, None).is_ok();
            let no_files = manifest(&out).is_empty();
            let vol_nonempty = !s.volumes.is_empty();
            rep.add(
                "empty_archive",
                ext_ok && no_files && vol_nonempty,
                format!(
                    "extract_ok={} no_files={} volumes={}",
                    ext_ok,
                    no_files,
                    s.volumes.len()
                ),
            );
        }
        Err(e) => rep.add("empty_archive", false, format!("pack error: {e}")),
    }
}

fn case_long_unicode(root: &Path, rep: &mut Report) {
    let long_dir = format!("directory_{}", "x".repeat(90));
    let files = vec![
        (format!("{long_dir}/unicode_파일_名前_テスト.txt"), 2000),
        (format!("{long_dir}/s3/{}/deep.bin", "y".repeat(70)), 30000),
    ];
    let (src, expect) = setup_sources(root, "unicode_src", &files, 505);
    let dest = root.join("unicode_src/arch.7z");
    let opts = PackOptions::default();
    let res =
        core::pack::pack_archive_split_with_options(&dest, std::slice::from_ref(&src), opts, 3072);
    match res {
        Ok(s) => {
            let out = root.join("unicode_src/out");
            let ext_ok = extract_all(&s.archive_path, &out, None).is_ok();
            let got = manifest(&out);
            let expect_archived = archived_expect(&src, &expect);
            let match_ok = ext_ok && got == expect_archived;
            rep.add(
                "long_unicode_names",
                match_ok,
                format!("extract_ok={} bytes_match={}", ext_ok, match_ok),
            );
        }
        Err(e) => rep.add("long_unicode_names", false, format!("pack error: {e}")),
    }
}

fn case_replace_many_to_fewer(root: &Path, rep: &mut Report) {
    let data_root = root.join("replace_m2f");
    let _ = fs::remove_dir_all(&data_root);
    fs::create_dir_all(&data_root).unwrap();

    // Source lives in its own subdirectory so the archive volumes (written next to it) never
    // become part of a later pack's own inventory.
    let src = data_root.join("src");
    fs::create_dir_all(&src).unwrap();
    let mut s = 606u64;
    fill_deterministic(&src.join("data.bin"), 300000, &mut s);
    fill_deterministic(&src.join("more.txt"), 4000, &mut s);
    let expect = manifest(&src);

    let base = data_root.join("arch");
    let dest = PathBuf::from(format!("{}.7z", base.display()));
    let opts = PackOptions::default();

    let first = core::pack::pack_archive_split_with_options(
        &dest,
        std::slice::from_ref(&src),
        opts.clone(),
        1024,
    );
    let first_ok = match &first {
        Ok(sum) => sum.volumes.len() >= 3,
        Err(_) => false,
    };

    // unrelated file must survive the tail-removal pass
    let bak = data_root.join("notes.bak");
    fs::write(&bak, b"unrelated backup").unwrap();

    let second = core::pack::pack_archive_split_with_options(
        &dest,
        std::slice::from_ref(&src),
        opts.clone(),
        262144,
    );
    let (tail_ok, preserved_ok) = match &second {
        Ok(sum) => {
            let n = sum.volumes.len();
            let tail = format!("{}.{:03}", base.display(), n + 1);
            let tail_gone = !Path::new(&tail).exists();
            (
                tail_gone,
                fs::read(&bak)
                    .map(|b| b == b"unrelated backup")
                    .unwrap_or(false),
            )
        }
        Err(_) => (false, false),
    };
    let out = data_root.join("out");
    let ext_ok = match &second {
        Ok(sum) => extract_all(&sum.archive_path, &out, None).is_ok(),
        Err(_) => false,
    };
    let got = manifest(&out);
    let expect_archived = archived_expect(&src, &expect);
    let match_ok = ext_ok && got == expect_archived;

    rep.add(
        "replace_many_to_fewer",
        first_ok && tail_ok && preserved_ok && match_ok,
        format!(
            "first_ok={} tail_removed={} bak_preserved={} extract_ok={} bytes_match={}",
            first_ok, tail_ok, preserved_ok, ext_ok, match_ok
        ),
    );
}

fn case_stale_tail_rollback(root: &Path, rep: &mut Report) {
    let dir = root.join("tail_rollback");
    fs::create_dir_all(&dir).unwrap();

    // Original set: arch.7z.001/002/003
    let mut s = 707u64;
    let mut originals: BTreeMap<String, String> = BTreeMap::new();
    for i in 1..=3 {
        let p = dir.join(format!("arch.7z.{i:03}"));
        fill_deterministic(&p, 1000 + i * 17, &mut s);
        originals.insert(format!("{i:03}"), hash_file(&p));
    }

    // Staged replacements for a shorter set (v1, v2): new content.
    let staged1 = dir.join(".staged_v1");
    let staged2 = dir.join(".staged_v2");
    fill_deterministic(&staged1, 500, &mut s);
    fill_deterministic(&staged2, 500, &mut s);

    // Inject a late commit failure: staged v2 is a directory -> stage_replacement must refuse.
    fs::remove_file(&staged2).unwrap();
    fs::create_dir_all(&staged2).unwrap();

    let dest1 = dir.join("arch.7z.001");
    let dest2 = dir.join("arch.7z.002");
    let stale = vec![dir.join("arch.7z.003")];

    let files = vec![
        (staged1.clone(), dest1.clone()),
        (staged2.clone(), dest2.clone()),
    ];
    let res = core::commit::FileTransaction::apply_with_removals(&files, &stale);

    let failed = res.is_err();
    // After rollback the original set must be intact and no new volumes installed.
    let restored = originals
        .iter()
        .all(|(k, h)| hash_file(&dir.join(format!("arch.7z.{k}"))) == *h);
    let new_is_original = dest1.exists() && hash_file(&dest1) == originals["001"];

    rep.add(
        "stale_tail_rollback",
        failed && restored && new_is_original,
        format!(
            "commit_failed={} restored={} original_restored={}",
            failed, restored, new_is_original
        ),
    );
}

fn case_directory_tail_refusal(root: &Path, rep: &mut Report) {
    let dir = root.join("dir_tail_refusal");
    fs::create_dir_all(&dir).unwrap();

    let mut s = 808u64;
    let v1 = dir.join("arch.7z.001");
    fill_deterministic(&v1, 500, &mut s);
    let v1_hash = hash_file(&v1);
    // A directory at a tail index must be refused, not deleted or overwritten.
    fs::create_dir_all(dir.join("arch.7z.002")).unwrap();

    let src = dir.join("src");
    fs::create_dir_all(&src).unwrap();
    let f = src.join("a.txt");
    fs::write(&f, b"data").unwrap();

    let dest = dir.join("arch.7z");
    let opts = PackOptions::default();
    let res = core::pack::pack_archive_split_with_options(
        &dest,
        std::slice::from_ref(&src),
        opts,
        262144,
    );
    let rejected = res.is_err();
    let v1_preserved = hash_file(&v1) == v1_hash;
    let no_new_tail = !dir.join("arch.7z.003").exists();
    rep.add(
        "directory_tail_refusal",
        rejected && v1_preserved && no_new_tail,
        format!(
            "rejected={} v1_preserved={} no_new_tail={}",
            rejected, v1_preserved, no_new_tail
        ),
    );
}

fn case_reject_non_7z_split(root: &Path, rep: &mut Report) {
    let dir = root.join("reject_non7z");
    fs::create_dir_all(&dir).unwrap();
    let src = dir.join("a.txt");
    fs::write(&src, b"data").unwrap();
    let dest = dir.join("arch.zip");
    let opts = PackOptions::default();
    let res =
        core::pack::pack_archive_split_with_options(&dest, std::slice::from_ref(&src), opts, 2048);
    let rejected = res.is_err();
    let no_output = !dest.exists();
    rep.add(
        "reject_non_7z_split",
        rejected && no_output,
        format!("rejected={} no_output={}", rejected, no_output),
    );
}

fn case_reject_zero_volume(root: &Path, rep: &mut Report) {
    let dir = root.join("reject_zero");
    fs::create_dir_all(&dir).unwrap();
    let src = dir.join("a.txt");
    fs::write(&src, b"data").unwrap();
    let dest = dir.join("arch.7z");
    let opts = PackOptions::default();
    let res =
        core::pack::pack_archive_split_with_options(&dest, std::slice::from_ref(&src), opts, 0);
    let rejected = res.is_err();
    let no_output = !dest.exists() && !dir.join("arch.7z.001").exists();
    rep.add(
        "reject_zero_volume",
        rejected && no_output,
        format!("rejected={} no_output={}", rejected, no_output),
    );
}
