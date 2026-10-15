//! Minimal runtime binding to `pdfium.dll` (bblanchon/pdfium-binaries, `chromium/7881`).
//!
//! Only the functions the bridge needs are bound. PDFium is not thread-safe: every call goes
//! through [`Pdfium::lock`].

use std::ffi::{c_char, c_int, c_ulong, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{HSTRING, PCSTR};

pub type Document = *mut c_void;
pub type Page = *mut c_void;
pub type Bitmap = *mut c_void;

pub const FPDF_ANNOT: c_int = 0x01;
pub const FPDF_PRINTING: c_int = 0x800;

/// PE machine type `pdfium.dll` must have to be loadable into this process.
#[cfg(target_arch = "x86_64")]
const EXPECTED_MACHINE: u16 = 0x8664; // IMAGE_FILE_MACHINE_AMD64
#[cfg(target_arch = "aarch64")]
const EXPECTED_MACHINE: u16 = 0xAA64; // IMAGE_FILE_MACHINE_ARM64

macro_rules! bind {
    ($($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        #[allow(non_snake_case)]
        pub struct Functions {
            $(pub $name: unsafe extern "system" fn($($arg),*) $(-> $ret)?,)*
        }

        impl Functions {
            fn load(module: windows::Win32::Foundation::HMODULE) -> Result<Self> {
                Ok(Self {
                    $($name: unsafe {
                        let symbol = GetProcAddress(
                            module,
                            PCSTR(concat!(stringify!($name), "\0").as_ptr()),
                        )
                        .ok_or_else(|| anyhow!("{} not found in pdfium.dll", stringify!($name)))?;
                        std::mem::transmute::<
                            unsafe extern "system" fn() -> isize,
                            unsafe extern "system" fn($($arg),*) $(-> $ret)?,
                        >(symbol)
                    },)*
                })
            }
        }
    };
}

bind! {
    FPDF_InitLibrary: fn();
    FPDF_GetLastError: fn() -> c_ulong;
    FPDF_LoadMemDocument64: fn(*const c_void, usize, *const c_char) -> Document;
    FPDF_CloseDocument: fn(Document);
    FPDF_GetPageCount: fn(Document) -> c_int;
    FPDF_LoadPage: fn(Document, c_int) -> Page;
    FPDF_ClosePage: fn(Page);
    FPDF_GetPageWidthF: fn(Page) -> f32;
    FPDF_GetPageHeightF: fn(Page) -> f32;
    FPDFPage_GetRotation: fn(Page) -> c_int;
    FPDF_RenderPage: fn(*mut c_void, Page, c_int, c_int, c_int, c_int, c_int, c_int) -> c_int;
    FPDF_RenderPageBitmap: fn(Bitmap, Page, c_int, c_int, c_int, c_int, c_int, c_int);
    FPDFBitmap_Create: fn(c_int, c_int, c_int) -> Bitmap;
    FPDFBitmap_FillRect: fn(Bitmap, c_int, c_int, c_int, c_int, u32) -> c_int;
    FPDFBitmap_GetBuffer: fn(Bitmap) -> *mut c_void;
    FPDFBitmap_GetStride: fn(Bitmap) -> c_int;
    FPDFBitmap_Destroy: fn(Bitmap);
}

pub struct Pdfium {
    functions: Functions,
    lock: Mutex<()>,
}

// The function pointers are plain code addresses; access to PDFium state is serialised by
// `lock`. The DLL is never unloaded.
unsafe impl Send for Pdfium {}
unsafe impl Sync for Pdfium {}

impl Pdfium {
    /// Serialise access to PDFium; call the returned functions only while holding it.
    pub fn lock(&self) -> (MutexGuard<'_, ()>, &Functions) {
        let guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        (guard, &self.functions)
    }
}

/// Description of `FPDF_GetLastError` codes.
pub fn error_text(code: c_ulong) -> &'static str {
    match code {
        2 => "file not found or could not be opened",
        3 => "not a PDF or corrupted",
        4 => "password protected",
        5 => "unsupported security scheme",
        6 => "page not found or content error",
        _ => "unknown error",
    }
}

static PDFIUM: OnceLock<Result<Pdfium, String>> = OnceLock::new();

/// `pdfium.dll` next to the executable.
pub fn library_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_default()
        .join("pdfium.dll")
}

/// Load and initialise PDFium on first use; later calls return the same result. Failing to
/// load only disables PDF printing.
pub fn get() -> Result<&'static Pdfium> {
    PDFIUM
        .get_or_init(|| load(&library_path()).map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| anyhow!("PDF printing is unavailable: {e}"))
}

fn load(path: &Path) -> Result<Pdfium> {
    if !path.is_file() {
        bail!("{} not found", path.display());
    }
    check_architecture(path)?;
    // Never freed: PDFium stays loaded for the life of the process.
    let module = unsafe { LoadLibraryW(&HSTRING::from(path.as_os_str())) }
        .with_context(|| format!("Cannot load {}", path.display()))?;
    let functions = Functions::load(module)?;
    unsafe { (functions.FPDF_InitLibrary)() };
    Ok(Pdfium {
        functions,
        lock: Mutex::new(()),
    })
}

/// Fails if the DLL at `path` exists but is not built for this process's architecture
/// (an x64 `pdfium.dll` next to the ARM64 exe or vice versa). A missing file passes.
pub fn check_architecture(path: &Path) -> Result<()> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("Cannot read {}", path.display())),
    };
    match pe_machine(&data) {
        Some(EXPECTED_MACHINE) => Ok(()),
        Some(machine) => bail!(
            "{} is built for {}, but this program is {}",
            path.display(),
            machine_name(machine),
            machine_name(EXPECTED_MACHINE)
        ),
        None => bail!("{} is not a valid DLL", path.display()),
    }
}

/// `IMAGE_FILE_HEADER::Machine` of a PE image.
fn pe_machine(data: &[u8]) -> Option<u16> {
    if data.get(..2)? != b"MZ" {
        return None;
    }
    let pe = u32::from_le_bytes(data.get(0x3C..0x40)?.try_into().ok()?) as usize;
    if data.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    Some(u16::from_le_bytes(
        data.get(pe + 4..pe + 6)?.try_into().ok()?,
    ))
}

fn machine_name(machine: u16) -> String {
    match machine {
        0x8664 => "x64".into(),
        0xAA64 => "ARM64".into(),
        0x014C => "x86".into(),
        other => format!("machine type {other:#06x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_executable_has_expected_machine() {
        let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        assert_eq!(pe_machine(&exe), Some(EXPECTED_MACHINE));
    }

    #[test]
    fn rejects_non_pe_data() {
        assert_eq!(pe_machine(b"MZ"), None);
        assert_eq!(pe_machine(b"%PDF-1.4"), None);
        let mut fake = vec![0u8; 0x48];
        fake[..2].copy_from_slice(b"MZ");
        fake[0x3C] = 0x40;
        fake[0x40..0x44].copy_from_slice(b"PE\0\0");
        fake[0x44..0x46].copy_from_slice(&0xAA64u16.to_le_bytes());
        assert_eq!(pe_machine(&fake), Some(0xAA64));
    }

    #[test]
    fn missing_library_passes_architecture_check() {
        assert!(check_architecture(Path::new("does-not-exist\\pdfium.dll")).is_ok());
    }

    #[test]
    fn library_loads() {
        get().expect("pdfium.dll next to the test binary (scripts\\fetch-pdfium.ps1)");
    }
}
