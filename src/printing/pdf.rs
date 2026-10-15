//! PDF documents rendered with PDFium (see [`super::pdfium`]).

use std::ffi::c_int;
use std::marker::PhantomData;

use anyhow::{Context, Result, bail};
use image::RgbaImage;
use windows::Win32::Graphics::Gdi::HDC;

use super::pdfium::{self, FPDF_ANNOT, FPDF_PRINTING, Pdfium};

/// Print-quality rendering including annotations (form field appearances, stamps, ...), as
/// PDFBox printed them.
const RENDER_FLAGS: c_int = FPDF_PRINTING | FPDF_ANNOT;

pub struct Pdf {
    pdfium: &'static Pdfium,
    doc: pdfium::Document,
    /// PDFium reads from this buffer for the life of the document.
    _data: Vec<u8>,
}

impl Pdf {
    pub fn load(bytes: &[u8]) -> Result<Self> {
        let pdfium = pdfium::get()?;
        let data = super::pdf_repair::repair(bytes).into_owned();
        let (_lock, f) = pdfium.lock();
        let doc = unsafe {
            (f.FPDF_LoadMemDocument64)(data.as_ptr().cast(), data.len(), std::ptr::null())
        };
        if doc.is_null() {
            let code = unsafe { (f.FPDF_GetLastError)() };
            bail!("Cannot open PDF document: {}", pdfium::error_text(code));
        }
        Ok(Self {
            pdfium,
            doc,
            _data: data,
        })
    }

    pub fn page_count(&self) -> Result<u32> {
        let (_lock, f) = self.pdfium.lock();
        Ok(unsafe { (f.FPDF_GetPageCount)(self.doc) }.max(0) as u32)
    }

    pub fn page(&self, index: u32) -> Result<Page<'_>> {
        let (_lock, f) = self.pdfium.lock();
        let page = unsafe { (f.FPDF_LoadPage)(self.doc, index as c_int) };
        if page.is_null() {
            bail!("Cannot load PDF page {}", index + 1);
        }
        Ok(Page {
            pdfium: self.pdfium,
            page,
            _doc: PhantomData,
        })
    }
}

impl Drop for Pdf {
    fn drop(&mut self) {
        let (_lock, f) = self.pdfium.lock();
        unsafe { (f.FPDF_CloseDocument)(self.doc) };
    }
}

/// A page; borrows its document because PDFium pages must be closed before the document.
pub struct Page<'a> {
    pdfium: &'static Pdfium,
    page: pdfium::Page,
    _doc: PhantomData<&'a Pdf>,
}

impl Page<'_> {
    /// Displayed page size in points (crop box with /Rotate applied).
    pub fn size_pt(&self) -> Result<(f64, f64)> {
        let (_lock, f) = self.pdfium.lock();
        unsafe {
            Ok((
                (f.FPDF_GetPageWidthF)(self.page) as f64,
                (f.FPDF_GetPageHeightF)(self.page) as f64,
            ))
        }
    }

    /// Crop box size in points, ignoring /Rotate. The Java bridge's auto-rotate decision
    /// (`getCropBox().getWidth() > getHeight()`) is based on this.
    pub fn crop_box_pt(&self) -> Result<(f64, f64)> {
        let (w, h) = self.size_pt()?;
        let rotation = {
            let (_lock, f) = self.pdfium.lock();
            unsafe { (f.FPDFPage_GetRotation)(self.page) }
        };
        Ok(if rotation % 2 == 1 { (h, w) } else { (w, h) })
    }

    /// Draw the page into `hdc` at `x`/`y` with `width` x `height` device pixels. PDFium
    /// emits GDI vector output (text, paths, images); the driver rasterises it.
    pub fn render_to_dc(&self, hdc: HDC, x: i32, y: i32, width: i32, height: i32) -> Result<()> {
        let (_lock, f) = self.pdfium.lock();
        let ok =
            unsafe { (f.FPDF_RenderPage)(hdc.0, self.page, x, y, width, height, 0, RENDER_FLAGS) };
        if ok == 0 {
            bail!("PDFium could not draw the page");
        }
        Ok(())
    }

    /// Render the horizontal slice `[row_start, row_end)` of the page rendered at
    /// `full_width` x `full_height` pixels. Only the slice is allocated.
    pub fn render_band(
        &self,
        full_width: u32,
        full_height: u32,
        row_start: u32,
        row_end: u32,
    ) -> Result<RgbaImage> {
        let rows = row_end - row_start;
        let (_lock, f) = self.pdfium.lock();
        unsafe {
            let bitmap = (f.FPDFBitmap_Create)(full_width as c_int, rows as c_int, 0);
            if bitmap.is_null() {
                bail!("Cannot allocate a {full_width} x {rows} bitmap");
            }
            let bitmap = BitmapGuard(bitmap, &f.FPDFBitmap_Destroy);
            (f.FPDFBitmap_FillRect)(
                bitmap.0,
                0,
                0,
                full_width as c_int,
                rows as c_int,
                0xFFFF_FFFF,
            );
            // The page is laid out at full size with its top edge `row_start` rows above the
            // bitmap; PDFium clips to the bitmap.
            (f.FPDF_RenderPageBitmap)(
                bitmap.0,
                self.page,
                0,
                -(row_start as c_int),
                full_width as c_int,
                full_height as c_int,
                0,
                RENDER_FLAGS,
            );

            // BGRx rows -> RGBA
            let stride = (f.FPDFBitmap_GetStride)(bitmap.0) as usize;
            let buffer = (f.FPDFBitmap_GetBuffer)(bitmap.0).cast::<u8>();
            if buffer.is_null() {
                bail!("PDFium returned no bitmap buffer");
            }
            let buffer = std::slice::from_raw_parts(buffer, stride * rows as usize);
            let mut rgba = Vec::with_capacity(full_width as usize * rows as usize * 4);
            for row in buffer.chunks_exact(stride) {
                for px in row[..full_width as usize * 4].as_chunks::<4>().0 {
                    rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
                }
            }
            RgbaImage::from_raw(full_width, rows, rgba).context("Unexpected bitmap size")
        }
    }
}

impl Drop for Page<'_> {
    fn drop(&mut self) {
        let (_lock, f) = self.pdfium.lock();
        unsafe { (f.FPDF_ClosePage)(self.page) };
    }
}

struct BitmapGuard<'a>(
    pdfium::Bitmap,
    &'a unsafe extern "system" fn(pdfium::Bitmap),
);

impl Drop for BitmapGuard<'_> {
    fn drop(&mut self) {
        unsafe { (self.1)(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One-page PDF with the given media box and content stream.
    pub(crate) fn simple_pdf(width: f64, height: f64, rotate: u32, content: &str) -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} {height}] /Rotate {rotate} \
                 /Contents 4 0 R >>"
            ),
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ),
        ];
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
        pdf.into_bytes()
    }

    #[test]
    fn bands_match_full_render() {
        // Anti-aliased diagonals cross every band boundary; odd sizes avoid lucky alignment.
        let pdf = simple_pdf(
            200.0,
            300.0,
            0,
            "1 0 0 RG 3 w 0 0 m 200 300 l S 0 300 m 200 0 l S",
        );
        let pdf = Pdf::load(&pdf).unwrap();
        let page = pdf.page(0).unwrap();
        let (w, h) = (333, 517);
        let full = page.render_band(w, h, 0, h).unwrap();
        // PDFium's anti-aliasing differs by a level or two when clipped; a misplaced band
        // (one row off) differs by ~150 on the diagonals.
        let max_diff = |a: &[u8], b: &[u8]| {
            assert_eq!(a.len(), b.len());
            a.iter()
                .zip(b)
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0)
        };
        for split in [1, 100, 258, 516] {
            let top = page.render_band(w, h, 0, split).unwrap();
            let bottom = page.render_band(w, h, split, h).unwrap();
            let (full_top, full_bottom) = full.as_raw().split_at(top.as_raw().len());
            assert!(
                max_diff(top.as_raw(), full_top) <= 2,
                "split {split}: top band"
            );
            assert!(
                max_diff(bottom.as_raw(), full_bottom) <= 2,
                "split {split}: bottom band"
            );
        }
    }

    #[test]
    fn rotated_page_size() {
        let pdf = Pdf::load(&simple_pdf(100.0, 200.0, 90, "")).unwrap();
        let page = pdf.page(0).unwrap();
        assert_eq!(page.size_pt().unwrap(), (200.0, 100.0));
        assert_eq!(page.crop_box_pt().unwrap(), (100.0, 200.0));
        let band = page.render_band(20, 10, 0, 10).unwrap();
        assert!(band.pixels().all(|p| p.0 == [255, 255, 255, 255]));
    }

    #[test]
    fn dc_render_matches_bitmap_render() {
        use windows::Win32::Graphics::Gdi::*;

        // Blue box (x 20..120, y 30..180 from the bottom) and a red diagonal.
        let pdf = simple_pdf(
            200.0,
            300.0,
            0,
            "0 0 1 rg 20 30 100 150 re f 1 0 0 RG 3 w 0 0 m 200 300 l S",
        );
        let pdf = Pdf::load(&pdf).unwrap();
        let page = pdf.page(0).unwrap();
        let (w, h, off_x, off_y) = (220usize, 320usize, 5usize, 7usize);

        let dc_pixels = unsafe {
            let dc = CreateCompatibleDC(None);
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w as i32,
                    biHeight: -(h as i32), // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bitmap =
                CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
            let old = SelectObject(dc, bitmap.into());
            let pixels = std::slice::from_raw_parts_mut(bits.cast::<u8>(), w * h * 4);
            pixels.fill(255);
            page.render_to_dc(dc, off_x as i32, off_y as i32, 200, 300)
                .unwrap();
            let _ = GdiFlush();
            let copy = pixels.to_vec();
            SelectObject(dc, old);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            copy
        };
        // BGRx at (x, y) of the DC bitmap
        let dc_rgb = |x: usize, y: usize| {
            let i = (y * w + x) * 4;
            [dc_pixels[i + 2], dc_pixels[i + 1], dc_pixels[i]]
        };

        assert_eq!(
            dc_rgb(off_x + 40, off_y + 140),
            [0, 0, 255],
            "inside the blue box"
        );
        assert_eq!(
            dc_rgb(off_x + 150, off_y + 20),
            [255, 255, 255],
            "page background"
        );
        assert_eq!(dc_rgb(2, 2), [255, 255, 255], "outside the page");

        // Same picture as the bitmap path, apart from edge pixels (anti-aliasing).
        let bitmap = page.render_band(200, 300, 0, 300).unwrap();
        let differing = bitmap
            .enumerate_pixels()
            .filter(|(x, y, p)| {
                let dc = dc_rgb(off_x + *x as usize, off_y + *y as usize);
                (0..3).any(|c| dc[c].abs_diff(p.0[c]) > 96)
            })
            .count();
        assert!(differing < 200 * 300 / 100, "{differing} pixels differ");
    }

    #[test]
    fn invalid_pdf_is_an_error() {
        let err = Pdf::load(b"not a pdf").err().unwrap();
        assert!(format!("{err:#}").contains("not a PDF"), "{err:#}");
    }
}
