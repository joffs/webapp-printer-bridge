//! Printing backends (blocking; call from a worker thread).

pub mod gdi;
pub mod pdf;
pub mod pdf_repair;
pub mod pdfium;
pub mod winspool;

use std::path::Path;

use anyhow::{Context, Result};

use crate::config::PrinterMapping;
use crate::protocol::Annotation;
use gdi::{GdiJob, Rect};

pub use winspool::{default_printer, list_printers, print_raw};

/// Keep rendered PDF bands (forceDPI) around this size to bound memory.
const BAND_BYTES: u64 = 32 * 1024 * 1024;

pub struct JobOptions<'a> {
    pub job_name: &'a str,
    pub copies: u32,
    pub mapping: &'a PrinterMapping,
    pub annotations: &'a [Annotation],
    /// Spool to this file instead of the device (testing).
    pub output_file: Option<&'a Path>,
}

/// Print an image stretched over the whole imageable area (like the Java `ImagePrintable`).
pub fn print_image(printer: &str, data: &[u8], opts: &JobOptions) -> Result<()> {
    let image = image::load_from_memory(data)
        .context("Cannot decode image")?
        .to_rgba8();
    let mut job = GdiJob::start(printer, opts.job_name, opts.copies, opts.output_file)?;
    for _ in 0..job.manual_copies {
        job.page(false, &mut |hdc, geometry| {
            let area = geometry.imageable(opts.mapping.reset_imageable_area);
            gdi::draw_rgba(hdc, area, image.width(), image.height(), image.as_raw())?;
            gdi::draw_annotations(hdc, geometry, area, opts.annotations);
            Ok(())
        })?;
    }
    job.finish()
}

/// Print all pages of a PDF: shrink-to-fit (never enlarge), centered in the imageable area,
/// optional auto-rotate and forced rasterisation DPI, as PDFBox `PDFPrintable` does.
pub fn print_pdf(printer: &str, data: &[u8], opts: &JobOptions) -> Result<()> {
    let pdf = pdf::Pdf::load(data)?;
    let mut job = GdiJob::start(printer, opts.job_name, opts.copies, opts.output_file)?;

    for index in 0..pdf.page_count()? {
        let page = pdf.page(index)?;
        let landscape = opts.mapping.auto_rotate && {
            let (w, h) = page.crop_box_pt()?;
            w > h
        };
        tracing::debug!("Page {index}: auto rotation landscape = {landscape}");

        for _ in 0..job.manual_copies {
            job.page(landscape, &mut |hdc, geometry| {
                let area = geometry.imageable(opts.mapping.reset_imageable_area);
                draw_pdf_page(hdc, geometry, area, &page, opts.mapping.force_dpi)?;
                gdi::draw_annotations(hdc, geometry, area, opts.annotations);
                Ok(())
            })?;
        }
    }
    job.finish()
}

fn draw_pdf_page(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    geometry: &gdi::PageGeometry,
    area: Rect,
    page: &pdf::Page,
    force_dpi: u32,
) -> Result<()> {
    let (page_w, page_h) = page.size_pt()?;
    let dest = fit_page(area, geometry.dpi_x, geometry.dpi_y, page_w, page_h);

    // Without forceDPI PDFium draws vector output straight into the printer DC and the
    // driver rasterises it at device resolution: sharp text and lines (no anti-aliasing
    // for 1-bit label printers to dither), small spool files.
    if force_dpi == 0 {
        let dest = snap_to_pixels(dest);
        return page.render_to_dc(
            hdc,
            dest.x as i32,
            dest.y as i32,
            dest.w as i32,
            dest.h as i32,
        );
    }

    // With forceDPI rasterise at that DPI relative to the page's own size and let GDI
    // scale (PDFPrintable `dpi` behaviour), in bands to bound memory.
    let render_w = ((page_w * force_dpi as f64 / 72.0).round() as u32).max(1);
    let render_h = ((page_h * force_dpi as f64 / 72.0).round() as u32).max(1);
    let rows_per_band = (BAND_BYTES / (render_w as u64 * 4)).clamp(1, render_h as u64) as u32;
    let mut row = 0;
    while row < render_h {
        let end = (row + rows_per_band).min(render_h);
        let band = page.render_band(render_w, render_h, row, end)?;
        let y0 = dest.y + dest.h * row as f64 / render_h as f64;
        let y1 = dest.y + dest.h * end as f64 / render_h as f64;
        let band_rect = Rect {
            x: dest.x,
            y: y0,
            w: dest.w,
            h: y1 - y0,
        };
        gdi::draw_rgba(hdc, band_rect, band.width(), band.height(), band.as_raw())?;
        row = end;
    }
    Ok(())
}

/// Scale a `page_w` x `page_h` point page into `area` (device pixels): shrink to fit,
/// never enlarge, centered.
pub fn fit_page(area: Rect, dpi_x: f64, dpi_y: f64, page_w: f64, page_h: f64) -> Rect {
    let area_w_pt = area.w * 72.0 / dpi_x;
    let area_h_pt = area.h * 72.0 / dpi_y;
    let scale = (area_w_pt / page_w).min(area_h_pt / page_h).min(1.0);
    let w = page_w * scale * dpi_x / 72.0;
    let h = page_h * scale * dpi_y / 72.0;
    Rect {
        x: area.x + (area.w - w) / 2.0,
        y: area.y + (area.h - h) / 2.0,
        w,
        h,
    }
}

/// Round the edges of `r` to whole device pixels.
fn snap_to_pixels(r: Rect) -> Rect {
    let (x, y) = (r.x.round(), r.y.round());
    Rect {
        x,
        y,
        w: ((r.x + r.w).round() - x).max(1.0),
        h: ((r.y + r.h).round() - y).max(1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_shrinks_and_centers() {
        // A4 sheet at 600 dpi, 1000x1000 pt page -> shrunk to the sheet width, centered vertically.
        let area = Rect {
            x: 0.0,
            y: 0.0,
            w: 4960.0,
            h: 7016.0,
        };
        let r = fit_page(area, 600.0, 600.0, 1000.0, 1000.0);
        assert!((r.w - 4960.0).abs() < 1e-6 && (r.h - 4960.0).abs() < 1e-6);
        assert!(r.x.abs() < 1e-6 && (r.y - (7016.0 - 4960.0) / 2.0).abs() < 1e-6);
    }

    #[test]
    fn fit_never_enlarges() {
        // 100x50 mm label on A4 stays 1:1 and is centered.
        let area = Rect {
            x: -50.0,
            y: -50.0,
            w: 4960.0,
            h: 7016.0,
        };
        let (w_pt, h_pt) = (283.46, 141.73);
        let r = fit_page(area, 600.0, 600.0, w_pt, h_pt);
        assert!((r.w - w_pt * 600.0 / 72.0).abs() < 1e-6);
        assert!((r.x - (-50.0 + (4960.0 - r.w) / 2.0)).abs() < 1e-9);
    }
}
