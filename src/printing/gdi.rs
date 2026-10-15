//! GDI print jobs: device context setup, page geometry, bitmap and text output.

use std::path::Path;

use anyhow::{Result, bail};
use windows::Win32::Foundation::{COLORREF, ERROR_CANCELLED, GetLastError};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Printing::DocumentPropertiesW;
use windows::Win32::Storage::Xps::*;
use windows::core::{HSTRING, PCWSTR};

use super::winspool::Printer;
use crate::protocol::Annotation;

/// Millimetres to points, same constant as the Java bridge.
const MM_TO_PT: f64 = 2.8346457;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Page metrics in device pixels. The DC origin is the top-left corner of the printable
/// area, so the physical paper starts at (-offset_x, -offset_y).
#[derive(Debug, Clone, Copy)]
pub struct PageGeometry {
    pub dpi_x: f64,
    pub dpi_y: f64,
    pub paper: Rect,
    pub printable: Rect,
}

impl PageGeometry {
    fn query(hdc: HDC) -> Self {
        let cap = |index| unsafe { GetDeviceCaps(Some(hdc), index) } as f64;
        Self {
            dpi_x: cap(LOGPIXELSX).max(1.0),
            dpi_y: cap(LOGPIXELSY).max(1.0),
            paper: Rect {
                x: -cap(PHYSICALOFFSETX),
                y: -cap(PHYSICALOFFSETY),
                w: cap(PHYSICALWIDTH),
                h: cap(PHYSICALHEIGHT),
            },
            printable: Rect {
                x: 0.0,
                y: 0.0,
                w: cap(HORZRES),
                h: cap(VERTRES),
            },
        }
    }

    /// Area content is fitted into. `resetImageableArea` uses the whole sheet (Java:
    /// `paper.setImageableArea(0, 0, width, height)`), otherwise the driver's printable area.
    pub fn imageable(&self, reset_imageable_area: bool) -> Rect {
        if reset_imageable_area {
            self.paper
        } else {
            self.printable
        }
    }
}

pub struct GdiJob {
    hdc: HDC,
    /// DEVMODEW plus driver-private bytes, u64 storage for alignment.
    devmode: Vec<u64>,
    landscape: bool,
    in_doc: bool,
    /// Copies the driver cannot produce itself; the caller repeats each page this often.
    pub manual_copies: u32,
}

impl GdiJob {
    /// Start a document on `printer` using the driver's default paper, portrait orientation.
    /// `output_file` redirects the spool output to a file (used for testing).
    pub fn start(
        printer: &str,
        job_name: &str,
        copies: u32,
        output_file: Option<&Path>,
    ) -> Result<Self> {
        let name = HSTRING::from(printer);
        let mut devmode = {
            let handle = Printer::open(printer)?;
            let size = unsafe { DocumentPropertiesW(None, handle.handle(), &name, None, None, 0) };
            if size <= 0 {
                bail!("Cannot read printer settings of {printer}");
            }
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            let r = unsafe {
                DocumentPropertiesW(
                    None,
                    handle.handle(),
                    &name,
                    Some(buf.as_mut_ptr().cast()),
                    None,
                    DM_OUT_BUFFER.0,
                )
            };
            if r < 0 {
                bail!("Cannot read printer settings of {printer}");
            }
            buf
        };

        let driver_copies =
            unsafe { DeviceCapabilitiesW(&name, PCWSTR::null(), DC_COPIES, None, None) };
        let manual_copies = {
            let dm = unsafe { &mut *devmode.as_mut_ptr().cast::<DEVMODEW>() };
            let fields = unsafe { &mut dm.Anonymous1.Anonymous1 };
            fields.dmOrientation = DMORIENT_PORTRAIT as i16;
            dm.dmFields |= DM_ORIENTATION;
            // Java prints uncollated copies (1,1,2,2); let the driver do it when it can.
            if driver_copies >= copies as i32 && copies <= i16::MAX as u32 {
                fields.dmCopies = copies as i16;
                dm.dmCollate = DMCOLLATE_FALSE;
                dm.dmFields |= DM_COPIES | DM_COLLATE;
                1
            } else {
                fields.dmCopies = 1;
                dm.dmFields |= DM_COPIES;
                copies
            }
        };

        let hdc = unsafe {
            CreateDCW(
                PCWSTR::null(),
                &name,
                PCWSTR::null(),
                Some(devmode.as_ptr().cast()),
            )
        };
        if hdc.is_invalid() {
            bail!("Cannot create device context for printer {printer}");
        }

        let mut job = Self {
            hdc,
            devmode,
            landscape: false,
            in_doc: false,
            manual_copies,
        };

        let doc_name = HSTRING::from(job_name);
        let output = output_file.map(|p| HSTRING::from(p.as_os_str()));
        let doc_info = DOCINFOW {
            cbSize: size_of::<DOCINFOW>() as i32,
            lpszDocName: PCWSTR(doc_name.as_ptr()),
            lpszOutput: output
                .as_ref()
                .map_or(PCWSTR::null(), |o| PCWSTR(o.as_ptr())),
            lpszDatatype: PCWSTR::null(),
            fwType: 0,
        };
        if unsafe { StartDocW(job.hdc, &doc_info) } <= 0 {
            if unsafe { GetLastError() } == ERROR_CANCELLED {
                bail!("Printing aborted");
            }
            return Err(windows::core::Error::from_thread().into());
        }
        job.in_doc = true;
        Ok(job)
    }

    /// Print one page. Orientation may change from page to page (PDF auto-rotate).
    pub fn page(
        &mut self,
        landscape: bool,
        draw: &mut dyn FnMut(HDC, &PageGeometry) -> Result<()>,
    ) -> Result<()> {
        if landscape != self.landscape {
            let dm = self.devmode.as_mut_ptr().cast::<DEVMODEW>();
            unsafe {
                (*dm).Anonymous1.Anonymous1.dmOrientation = if landscape {
                    DMORIENT_LANDSCAPE
                } else {
                    DMORIENT_PORTRAIT
                } as i16;
                if ResetDCW(self.hdc, dm).is_invalid() {
                    bail!("Cannot change page orientation");
                }
            }
            self.landscape = landscape;
        }

        if unsafe { StartPage(self.hdc) } <= 0 {
            return Err(windows::core::Error::from_thread().into());
        }
        let geometry = PageGeometry::query(self.hdc);
        let drawn = draw(self.hdc, &geometry);
        if unsafe { EndPage(self.hdc) } <= 0 && drawn.is_ok() {
            if unsafe { GetLastError() } == ERROR_CANCELLED {
                bail!("Printing aborted");
            }
            return Err(windows::core::Error::from_thread().into());
        }
        drawn
    }

    pub fn finish(mut self) -> Result<()> {
        self.in_doc = false;
        if unsafe { EndDoc(self.hdc) } <= 0 {
            return Err(windows::core::Error::from_thread().into());
        }
        Ok(())
    }
}

impl Drop for GdiJob {
    fn drop(&mut self) {
        unsafe {
            if self.in_doc {
                AbortDoc(self.hdc);
            }
            let _ = DeleteDC(self.hdc);
        }
    }
}

/// Draw an RGBA bitmap stretched into `dest` (device pixels). Transparent pixels are
/// composited onto white, like Java2D drawing on paper.
pub fn draw_rgba(hdc: HDC, dest: Rect, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
    if width == 0 || height == 0 {
        return Ok(());
    }
    let stride = (width as usize * 3).div_ceil(4) * 4;
    let mut bgr = vec![0u8; stride * height as usize];
    for (row_in, row_out) in rgba
        .chunks_exact(width as usize * 4)
        .zip(bgr.chunks_exact_mut(stride))
    {
        for (px, out) in row_in
            .as_chunks::<4>()
            .0
            .iter()
            .zip(row_out.as_chunks_mut::<3>().0)
        {
            let a = px[3] as u32;
            let blend = |c: u8| ((c as u32 * a + 255 * (255 - a)) / 255) as u8;
            out[0] = blend(px[2]);
            out[1] = blend(px[1]);
            out[2] = blend(px[0]);
        }
    }

    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32), // top-down
            biPlanes: 1,
            biBitCount: 24,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let (x, y) = (dest.x.round() as i32, dest.y.round() as i32);
    let w = (dest.x + dest.w).round() as i32 - x;
    let h = (dest.y + dest.h).round() as i32 - y;
    unsafe {
        SetStretchBltMode(hdc, HALFTONE);
        let _ = SetBrushOrgEx(hdc, 0, 0, None);
        let lines = StretchDIBits(
            hdc,
            x,
            y,
            w,
            h,
            0,
            0,
            width as i32,
            height as i32,
            Some(bgr.as_ptr().cast()),
            &info,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
        if lines == 0 {
            bail!("Drawing bitmap on printer failed");
        }
    }
    Ok(())
}

/// Draw text annotations. Positions are millimetres from the top-left of the imageable
/// area to the text baseline; font size is in points, black "Sans-Serif" (Arial).
pub fn draw_annotations(
    hdc: HDC,
    geometry: &PageGeometry,
    origin: Rect,
    annotations: &[Annotation],
) {
    unsafe {
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(0));
        SetTextAlign(hdc, TA_BASELINE | TA_LEFT);
    }
    for annotation in annotations {
        let Some(text) = annotation.text.as_deref() else {
            tracing::warn!("annotation text is null");
            continue;
        };
        let x = origin.x + annotation.x.unwrap_or(0.0) * MM_TO_PT * geometry.dpi_x / 72.0;
        let y = origin.y + annotation.y.unwrap_or(0.0) * MM_TO_PT * geometry.dpi_y / 72.0;
        let size_pt = annotation.size.unwrap_or(10.0);
        let height = -(size_pt * geometry.dpi_y / 72.0).round() as i32;
        let weight = if annotation.bold {
            FW_BOLD.0
        } else {
            FW_NORMAL.0
        } as i32;

        unsafe {
            let font = CreateFontW(
                height,
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_TT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                0,
                &HSTRING::from("Arial"),
            );
            let previous = SelectObject(hdc, font.into());
            let wide: Vec<u16> = text.encode_utf16().collect();
            let _ = TextOutW(hdc, x.round() as i32, y.round() as i32, &wide);
            SelectObject(hdc, previous);
            let _ = DeleteObject(font.into());
        }
    }
}
