//! Dev tool: print a PDF/image/raw file through the same code path as the bridge.
//! Usage: cargo run --example print_file -- <printer> <file> [--out spool.pdf] [--rotate]
//!        [--no-reset] [--dpi N] [--copies N] [--annotate] [--raw]

use std::path::PathBuf;

use printer_bridge::config::PrinterMapping;
use printer_bridge::printing::{self, JobOptions};
use printer_bridge::protocol::Annotation;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    let printer = &args[0];
    let file = PathBuf::from(&args[1]);
    let data = std::fs::read(&file)?;
    let mapping = PrinterMapping {
        auto_rotate: flag("--rotate"),
        reset_imageable_area: !flag("--no-reset"),
        force_dpi: value("--dpi").map_or(0, |v| v.parse().unwrap()),
        ..Default::default()
    };
    let annotations = if flag("--annotate") {
        vec![
            Annotation {
                text: Some("ANNOT 10/20mm 12pt".into()),
                x: Some(10.0),
                y: Some(20.0),
                size: Some(12.0),
                bold: false,
            },
            Annotation {
                text: Some("BOLD 10/40mm 24pt".into()),
                x: Some(10.0),
                y: Some(40.0),
                size: Some(24.0),
                bold: true,
            },
        ]
    } else {
        Vec::new()
    };
    let out = value("--out").map(|p| std::path::absolute(p).unwrap());
    let opts = JobOptions {
        job_name: "print_file example",
        copies: value("--copies").map_or(1, |v| v.parse().unwrap()),
        mapping: &mapping,
        annotations: &annotations,
        output_file: out.as_deref(),
    };

    let started = std::time::Instant::now();
    let name = file.to_string_lossy().to_lowercase();
    if flag("--raw") {
        printing::print_raw(printer, "print_file example", &data)?;
    } else if name.ends_with(".pdf") {
        printing::print_pdf(printer, &data, &opts)?;
    } else {
        printing::print_image(printer, &data, &opts)?;
    }
    println!("printed in {} ms", started.elapsed().as_millis());
    Ok(())
}
