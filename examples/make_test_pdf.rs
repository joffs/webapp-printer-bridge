//! Dev tool: write a small multi-page PDF used to check fitting, centering and rotation.
//! Usage: cargo run --example make_test_pdf -- out.pdf

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "test.pdf".into());

    // (width pt, height pt, /Rotate, label)
    let pages: &[(f64, f64, u32, &str)] = &[
        (595.0, 842.0, 0, "1 A4 PORTRAIT"),
        (842.0, 595.0, 0, "2 A4 LANDSCAPE"),
        (283.46, 141.73, 0, "3 LABEL 100x50mm"),
        (595.0, 842.0, 90, "4 A4 ROTATE90"),
    ];
    let barcode_only = std::env::args().any(|a| a == "--barcode");
    // 62 x 29 mm label with a Code128-like bar pattern (module 0.33 mm, not dot-aligned).
    let barcode_pages: &[(f64, f64, u32, &str)] = &[(175.75, 82.2, 0, "BARCODE 62x29")];
    let pages = if barcode_only { barcode_pages } else { pages };

    let mut objects: Vec<String> = Vec::new();
    // 1: catalog, 2: pages, 3: font, then page/content pairs.
    let kids: Vec<String> = (0..pages.len())
        .map(|i| format!("{} 0 R", 4 + i * 2))
        .collect();
    objects.push("<< /Type /Catalog /Pages 2 0 R >>".into());
    objects.push(format!(
        "<< /Type /Pages /Kids [{}] /Count {} >>",
        kids.join(" "),
        pages.len()
    ));
    objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold >>".into());

    for (i, &(w, h, rotate, label)) in pages.iter().enumerate() {
        let content_id = 5 + i * 2;
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Rotate {rotate} \
             /Resources << /Font << /F1 3 0 R >> >> /Contents {content_id} 0 R >>"
        ));
        let size = (h.min(w) / 12.0).max(8.0);
        if barcode_only {
            let module = 0.33 * 72.0 / 25.4;
            let pattern = [
                2, 1, 1, 2, 2, 2, 2, 2, 1, 2, 2, 2, 1, 3, 1, 1, 2, 3, 3, 1, 1, 1, 2, 2, 1, 1, 3, 1,
                2, 2,
            ];
            let mut x = 10.0;
            let mut bars = String::new();
            for (k, &m) in pattern.iter().cycle().take(90).enumerate() {
                let width = m as f64 * module;
                if k % 2 == 0 {
                    bars.push_str(&format!("{x:.3} 18 {width:.3} 50 re f "));
                }
                x += width;
            }
            let stream = format!("0 g {bars} BT /F1 8 Tf 10 6 Td (123456789012) Tj ET");
            objects.push(format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            ));
            continue;
        }
        let stream = format!(
            "4 w 0 0 1 RG 2 2 {bw} {bh} re S \
             1 0 0 RG 0 0 m {w} {h} l S 0 {h} m {w} 0 l S \
             0 g 0 0 m 30 0 l 0 30 l f \
             BT /F1 {size} Tf 12 {ty} Td ({label}) Tj ET",
            bw = w - 4.0,
            bh = h - 4.0,
            ty = h - size - 10.0,
        );
        objects.push(format!(
            "<< /Length {} >>\nstream\n{stream}\nendstream",
            stream.len()
        ));
    }

    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{body}\nendobj\n", i + 1));
    }
    let xref = pdf.len();
    pdf.push_str(&format!(
        "xref\n0 {}\n0000000000 65535 f \n",
        objects.len() + 1
    ));
    for offset in offsets {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    ));
    std::fs::write(&out, pdf).unwrap();
    println!("wrote {out}");
}
