//! Dev tool: render every PDF page to PNG with the bridge's PDF renderer and print page sizes.
//! Usage: cargo run --example render_pdf -- in.pdf out_prefix [width_px]

use printer_bridge::printing::pdf::Pdf;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let pdf = Pdf::load(&std::fs::read(&args[0])?)?;
    let target_w: f64 = args.get(2).map_or(400.0, |v| v.parse().unwrap());

    for i in 0..pdf.page_count()? {
        let page = pdf.page(i)?;
        let (w, h) = page.size_pt()?;
        let (cw, ch) = page.crop_box_pt()?;
        let px_w = target_w as u32;
        let px_h = (target_w * h / w).round() as u32;
        // Render in two bands to exercise the banding path.
        let top = page.render_band(px_w, px_h, 0, px_h / 2)?;
        let bottom = page.render_band(px_w, px_h, px_h / 2, px_h)?;
        let mut full = image::RgbaImage::new(px_w, px_h);
        image::imageops::replace(&mut full, &top, 0, 0);
        image::imageops::replace(&mut full, &bottom, 0, (px_h / 2) as i64);
        let out = format!("{}-{}.png", args[1], i + 1);
        full.save(&out)?;
        println!(
            "page {}: size {w:.1} x {h:.1} pt, crop {cw:.1} x {ch:.1} pt -> {out}",
            i + 1
        );
    }
    Ok(())
}
