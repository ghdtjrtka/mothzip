use crate::core::extract::ExtractionSummary;
use crate::core::inspect::PeekSummary;
use crate::core::pack::PackSummary;

pub fn print_bytes_human(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.2} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

pub fn report_peek(summary: &PeekSummary, json_mode: bool) {
    if json_mode {
        if let Ok(json) = serde_json::to_string_pretty(summary) {
            println!("{}", json);
        }
        return;
    }

    println!("==================================================");
    println!(" Archive: {}", summary.archive_name);
    println!(" Total entries: {}", summary.total_entries);
    println!(
        " Size: {} -> {} (Ratio: {})",
        print_bytes_human(summary.total_uncompressed_bytes),
        print_bytes_human(summary.total_compressed_bytes),
        summary.compression_ratio
    );

    if summary.has_single_root {
        println!(
            " Structure: Single root directory ['{}']",
            summary.root_name.as_deref().unwrap_or("?")
        );
    } else {
        println!(" Structure: Multiple files / directories at root");
    }

    println!(" Extensions:");
    for (ext, count) in &summary.extension_distribution {
        println!("   {:10} : {:4} files", ext, count);
    }

    println!(" Tree summary:");
    for item in &summary.tree_summary {
        if item.is_dir {
            println!("   📁 {}/", item.path.trim_end_matches('/'));
        } else {
            println!("   📄 {:40} ({})", item.path, print_bytes_human(item.size));
        }
    }

    if summary.truncated_items > 0 {
        println!("   ... ({} more items omitted)", summary.truncated_items);
    }
    println!("==================================================");
}

pub fn report_extract(summary: &ExtractionSummary, json_mode: bool, quiet: bool) {
    if json_mode {
        if let Ok(json) = serde_json::to_string_pretty(summary) {
            println!("{}", json);
        }
        return;
    }

    if quiet {
        return;
    }

    println!(
        "✓ Extracted {} files ({}) into: {}",
        summary.extracted_files,
        print_bytes_human(summary.extracted_bytes),
        summary.target_dir.display()
    );

    if summary.smart_adjusted {
        println!("  (Smart Unpack: Created subfolder to prevent cluttering)");
    }
}

pub fn report_pack(summary: &PackSummary, json_mode: bool, quiet: bool) {
    if json_mode {
        if let Ok(json) = serde_json::to_string_pretty(summary) {
            println!("{}", json);
        }
        return;
    }

    if quiet {
        return;
    }

    println!(
        "✓ Packed {} files into: {} ({} -> {}, Ratio: {})",
        summary.total_files,
        summary.archive_path.display(),
        print_bytes_human(summary.total_uncompressed_bytes),
        print_bytes_human(summary.compressed_bytes),
        summary.compression_ratio
    );
}
