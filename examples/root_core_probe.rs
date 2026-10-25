#[path = "../src/core/mod.rs"]
pub mod core;

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let path = std::path::Path::new(&args[2]);
    let result = match args[1].as_str() {
        "catalog" => core::catalog::read_archive_catalog(
            path,
            None,
            core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .map(|entries| serde_json::to_string(&entries).unwrap()),
        "test" => core::inspect::test_archive_integrity(
            path,
            None,
            core::guard::DEFAULT_MAX_UNCOMPRESSED_BYTES,
        )
        .map(|summary| serde_json::to_string(&summary).unwrap()),
        _ => Err("unknown probe operation".into()),
    };
    match result {
        Ok(json) => println!("{}", json),
        Err(error) => {
            eprintln!("{}", serde_json::json!({"error":error.to_string()}));
            std::process::exit(1);
        }
    }
}
