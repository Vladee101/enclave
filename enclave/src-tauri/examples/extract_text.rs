//! Run ingestion's text extraction on a file and print what it yields —
//! for checking a document that fails to ingest, or ingests as garbage,
//! without going through upload and the worker.
//!
//!   cargo run --example extract_text -- <path> [--full]
//!
//! Prints the character count, how many 512-character chunks it would
//! make, and the start of the text (all of it with --full).

use enclave_lib::ingest::{
    extract::{extract, Layout},
    split_into_chunks, split_rows,
};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or_else(|| anyhow::anyhow!("usage: extract_text <path> [--full]"))?;
    let full = args.any(|a| a == "--full");

    let bytes = std::fs::read(&path)?;
    let name = std::path::Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();

    let started = std::time::Instant::now();
    let extracted = extract(&bytes, &name)?;
    let elapsed = started.elapsed();
    // Same chunking as ingestion, so the count is what the worker would embed.
    let chunks = match extracted.layout {
        Layout::Prose => split_into_chunks(&extracted.text, 512, 64),
        Layout::Rows => split_rows(&extracted.text, 512),
    };
    let text = extracted.text;

    let chars = text.chars().count();
    println!(
        "{name}: {} bytes -> {chars} chars ({:?}), {} chunks, {:.0} ms",
        bytes.len(),
        extracted.layout,
        chunks.len(),
        elapsed.as_secs_f64() * 1000.0
    );
    println!("----");
    if full {
        println!("{text}");
    } else {
        println!("{}", text.chars().take(600).collect::<String>());
        if chars > 600 {
            println!("… ({} more chars; --full to see all)", chars - 600);
        }
    }
    Ok(())
}
