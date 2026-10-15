//! Print spooler access: printer enumeration and RAW jobs.

use anyhow::{Result, bail};
use windows::Win32::Graphics::Printing::*;
use windows::core::{HSTRING, PCWSTR, PWSTR, w};

/// Owned printer handle, closed on drop.
pub struct Printer(PRINTER_HANDLE);

impl Printer {
    pub fn open(name: &str) -> Result<Self> {
        let mut handle = PRINTER_HANDLE::default();
        unsafe { OpenPrinterW(&HSTRING::from(name), &mut handle, None)? };
        Ok(Self(handle))
    }

    pub fn handle(&self) -> PRINTER_HANDLE {
        self.0
    }
}

impl Drop for Printer {
    fn drop(&mut self) {
        unsafe {
            let _ = ClosePrinter(self.0);
        }
    }
}

/// Names of all local and connected (network) printers, as shown in Windows.
pub fn list_printers() -> Result<Vec<String>> {
    let flags = PRINTER_ENUM_LOCAL | PRINTER_ENUM_CONNECTIONS;
    let (mut needed, mut returned) = (0u32, 0u32);
    unsafe {
        let _ = EnumPrintersW(flags, PCWSTR::null(), 4, None, &mut needed, &mut returned);
    }
    if needed == 0 {
        return Ok(Vec::new());
    }

    // u64 storage keeps the PRINTER_INFO_4W array correctly aligned.
    let mut storage = vec![0u64; (needed as usize).div_ceil(8)];
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(storage.as_mut_ptr().cast::<u8>(), needed as usize)
    };
    unsafe {
        EnumPrintersW(
            flags,
            PCWSTR::null(),
            4,
            Some(bytes),
            &mut needed,
            &mut returned,
        )?
    };

    let infos = unsafe {
        std::slice::from_raw_parts(
            storage.as_ptr().cast::<PRINTER_INFO_4W>(),
            returned as usize,
        )
    };
    Ok(infos
        .iter()
        .filter(|info| !info.pPrinterName.is_null())
        .map(|info| unsafe { info.pPrinterName.to_string().unwrap_or_default() })
        .collect())
}

pub fn default_printer() -> Option<String> {
    let mut len = 0u32;
    unsafe {
        let _ = GetDefaultPrinterW(None, &mut len);
    }
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    let ok = unsafe { GetDefaultPrinterW(Some(PWSTR(buf.as_mut_ptr())), &mut len) };
    if !ok.as_bool() {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// Send bytes to the printer untouched (datatype RAW), e.g. ESC/POS or ZPL.
pub fn print_raw(printer: &str, job_name: &str, data: &[u8]) -> Result<()> {
    let printer = Printer::open(printer)?;
    let mut doc_name: Vec<u16> = job_name.encode_utf16().chain(Some(0)).collect();
    let doc_info = DOC_INFO_1W {
        pDocName: PWSTR(doc_name.as_mut_ptr()),
        pOutputFile: PWSTR::null(),
        pDatatype: PWSTR(w!("RAW").as_ptr().cast_mut()),
    };

    unsafe {
        if StartDocPrinterW(printer.handle(), 1, &doc_info) == 0 {
            return Err(windows::core::Error::from_thread().into());
        }
        let result = write_page(&printer, data);
        let _ = EndDocPrinter(printer.handle());
        result
    }
}

unsafe fn write_page(printer: &Printer, data: &[u8]) -> Result<()> {
    unsafe {
        if !StartPagePrinter(printer.handle()).as_bool() {
            return Err(windows::core::Error::from_thread().into());
        }
        let mut offset = 0usize;
        while offset < data.len() {
            let chunk = &data[offset..data.len().min(offset + (1 << 20))];
            let mut written = 0u32;
            if !WritePrinter(
                printer.handle(),
                chunk.as_ptr().cast(),
                chunk.len() as u32,
                &mut written,
            )
            .as_bool()
            {
                let err = windows::core::Error::from_thread();
                let _ = EndPagePrinter(printer.handle());
                return Err(err.into());
            }
            if written == 0 {
                let _ = EndPagePrinter(printer.handle());
                bail!("Printer accepted no data");
            }
            offset += written as usize;
        }
        let _ = EndPagePrinter(printer.handle());
    }
    Ok(())
}
