//! Build a new PDF from only the selected pages, copying just what those pages
//! need to keep their visual + semantic meaning — no orphan objects, and
//! duplicate objects (a font/image shared across pages) collapsed to one.
//!
//! Usage:
//!   cargo run --release --example subset_pages \
//!       -- <input.pdf> <output.pdf> <page> [page ...]
//!
//! Pages are 0-indexed and kept in the order given.

use pdf_oxide::editor::DocumentEditor;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("Usage: subset_pages <input.pdf> <output.pdf> <page> [page ...]");
        std::process::exit(1);
    }
    let input = &args[1];
    let output = &args[2];
    let pages: Vec<usize> = args[3..]
        .iter()
        .map(|s| {
            s.parse()
                .expect("page indices must be non-negative integers")
        })
        .collect();

    let mut editor = match DocumentEditor::from_bytes(std::fs::read(input).expect("read input")) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("ERROR: cannot open {input}: {e}");
            std::process::exit(1);
        },
    };

    let (bytes, report) = match editor.subset_pages_with_options(&pages, Default::default()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ERROR: subset failed: {e}");
            std::process::exit(1);
        },
    };

    std::fs::write(output, &bytes).expect("write output");
    println!(
        "Wrote {output}: {} pages, {} bytes, {} objects ({} deduplicated).",
        pages.len(),
        bytes.len(),
        report.objects_written,
        report.objects_deduped
    );
    for warning in &report.warnings {
        println!("  warning: {warning}");
    }
}
