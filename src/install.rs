//! Per-user installation and autostart, without an external setup program and without admin
//! rights:
//!
//! * install: copy the program files to `%LOCALAPPDATA%\Programs\WebApp Printer Bridge\`,
//!   take over `config.json` from the folder the exe was started from, create a Start menu
//!   shortcut, register in "Apps & features" (uninstall = `printer-bridge.exe --uninstall`)
//!   and enable autostart;
//! * autostart: value in `HKCU\...\CurrentVersion\Run` pointing to the running exe;
//! * uninstall: remove all of the above; the files are deleted after the process has exited.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IPersistFile,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegDeleteValueW, RegGetValueW, RegSetValueExW,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
};
use windows::core::{BOOL, HSTRING, Interface, PCWSTR};

use crate::{APP_NAME, VERSION};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const UNINSTALL_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Uninstall\WebAppPrinterBridge";
/// Window class of the tray window; identifies running bridge instances.
pub const TRAY_CLASS: &str = "WebAppPrinterBridgeTray";

/// Files that make up the program (data such as config.json, log\ and downloads\ is not
/// part of it).
const PROGRAM_FILES: &[&str] = &["printer-bridge.exe", "pdfium.dll", "README.md", "LICENSE"];
const PROGRAM_DIRS: &[&str] = &["licenses"];

pub fn install_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_default();
    base.join("Programs").join(APP_NAME)
}

fn start_menu_shortcut() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_default();
    base.join(r"Microsoft\Windows\Start Menu\Programs")
        .join(format!("{APP_NAME}.lnk"))
}

fn current_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_default()
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        std::path::absolute(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .trim_end_matches('\\')
            .to_lowercase()
    };
    norm(a) == norm(b)
}

/// Whether this process runs from the install folder.
pub fn is_installed_copy() -> bool {
    current_exe()
        .parent()
        .is_some_and(|dir| same_path(dir, &install_dir()))
}

/// Version recorded by the last installation ("Apps & features" entry), if installed.
pub fn installed_version() -> Option<String> {
    reg_get_string(UNINSTALL_KEY, "DisplayVersion")
}

// --- autostart ---

/// Whether Windows starts *this* exe at logon.
pub fn autostart_enabled() -> bool {
    reg_get_string(RUN_KEY, APP_NAME)
        .is_some_and(|cmd| same_path(Path::new(cmd.trim_matches('"')), &current_exe()))
}

/// Start this exe at logon (or stop doing so).
pub fn set_autostart(enabled: bool) -> Result<()> {
    set_autostart_for(&current_exe(), enabled)
}

fn set_autostart_for(exe: &Path, enabled: bool) -> Result<()> {
    if enabled {
        reg_set_string(RUN_KEY, APP_NAME, &format!("\"{}\"", exe.display()))?;
        // An entry disabled in Task Manager stays disabled until this flag is removed.
        reg_delete_value(STARTUP_APPROVED_KEY, APP_NAME);
    } else {
        reg_delete_value(RUN_KEY, APP_NAME);
        reg_delete_value(STARTUP_APPROVED_KEY, APP_NAME);
    }
    tracing::info!("Autostart {}", if enabled { "enabled" } else { "disabled" });
    Ok(())
}

// --- running instances ---

/// Tray windows of bridge processes (other than this one) whose exe lives in `dir`.
fn instances_in(dir: &Path) -> Vec<(HWND, u32)> {
    struct Search {
        found: Vec<(HWND, u32)>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        let mut class = [0u16; 64];
        let len = unsafe { GetClassNameW(hwnd, &mut class) } as usize;
        if String::from_utf16_lossy(&class[..len]) == TRAY_CLASS {
            let mut pid = 0;
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            search.found.push((hwnd, pid));
        }
        BOOL(1)
    }
    let mut search = Search { found: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut search as *mut _ as isize));
    }
    let own = std::process::id();
    search
        .found
        .into_iter()
        .filter(|&(_, pid)| pid != own)
        .filter(|&(_, pid)| {
            process_path(pid)
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .is_some_and(|d| same_path(&d, dir))
        })
        .collect()
}

fn process_path(pid: u32) -> Option<PathBuf> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(process);
        ok.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(
            &buf[..len as usize],
        )))
    }
}

/// Whether a bridge from the install folder is running (and this process is not it).
pub fn installed_instance_running() -> bool {
    !is_installed_copy() && !instances_in(&install_dir()).is_empty()
}

/// Close bridge instances running from `dir` (like tray -> Exit) and wait until they are gone.
fn stop_instances_in(dir: &Path) {
    for (hwnd, pid) in instances_in(dir) {
        tracing::info!("Closing running instance (pid {pid})");
        unsafe {
            let process = OpenProcess(PROCESS_SYNCHRONIZE, false, pid).ok();
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            if let Some(process) = process {
                WaitForSingleObject(process, 15_000);
                let _ = CloseHandle(process);
            }
        }
    }
}

/// Wait until the exe in `dir` can be overwritten (it stays locked for a moment after its
/// process has ended).
fn wait_until_replaceable(dir: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let exe = dir.join("printer-bridge.exe");
    while exe.exists() && std::fs::OpenOptions::new().write(true).open(&exe).is_err() {
        if Instant::now() > deadline {
            bail!("{} is still in use", exe.display());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

// --- install / uninstall ---

/// Install (or update) the program for the current user and return the installed exe.
/// The caller must have stopped its own server, starts the installed exe and exits.
pub fn install() -> Result<PathBuf> {
    let source = current_exe()
        .parent()
        .map(Path::to_path_buf)
        .context("exe folder unknown")?;
    let target = install_dir();
    if same_path(&source, &target) {
        bail!("This copy is already the installed one.");
    }
    if !source.join("pdfium.dll").is_file() {
        bail!(
            "pdfium.dll is missing next to printer-bridge.exe.\n\
             Extract the whole zip before installing."
        );
    }
    tracing::info!("Installing {VERSION} to {}", target.display());

    stop_instances_in(&target);
    wait_until_replaceable(&target)?;
    std::fs::create_dir_all(&target)
        .with_context(|| format!("Cannot create {}", target.display()))?;
    for name in PROGRAM_FILES {
        let from = source.join(name);
        if from.is_file() {
            std::fs::copy(&from, target.join(name))
                .with_context(|| format!("Cannot copy {name}"))?;
        }
    }
    for name in PROGRAM_DIRS {
        copy_dir(&source.join(name), &target.join(name))?;
    }
    // Take over the settings of the copy being installed (e.g. extracted into an old
    // installation folder), but never overwrite those of an existing installation.
    let config = source.join("config.json");
    if config.is_file() && !target.join("config.json").exists() {
        std::fs::copy(&config, target.join("config.json")).context("Cannot copy config.json")?;
    }

    let exe = target.join("printer-bridge.exe");
    create_shortcut(&start_menu_shortcut(), &exe, &target)?;
    register_uninstall(&exe, &target)?;
    // Autostart moves to the installed copy (replaces an entry for this portable copy).
    set_autostart_for(&exe, true)?;
    Ok(exe)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    if !from.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

fn register_uninstall(exe: &Path, dir: &Path) -> Result<()> {
    let size_kb = PROGRAM_FILES
        .iter()
        .filter_map(|n| std::fs::metadata(dir.join(n)).ok())
        .map(|m| m.len())
        .sum::<u64>()
        / 1024;
    let exe = exe.display();
    reg_set_string(UNINSTALL_KEY, "DisplayName", APP_NAME)?;
    reg_set_string(UNINSTALL_KEY, "DisplayVersion", VERSION)?;
    reg_set_string(UNINSTALL_KEY, "DisplayIcon", &format!("{exe},0"))?;
    reg_set_string(UNINSTALL_KEY, "InstallLocation", &dir.display().to_string())?;
    reg_set_string(
        UNINSTALL_KEY,
        "UninstallString",
        &format!("\"{exe}\" --uninstall"),
    )?;
    reg_set_dword(UNINSTALL_KEY, "NoModify", 1)?;
    reg_set_dword(UNINSTALL_KEY, "NoRepair", 1)?;
    reg_set_dword(UNINSTALL_KEY, "EstimatedSize", size_kb as u32)?;
    Ok(())
}

/// Remove the installation. With `remove_data` also config.json, logs and downloads.
/// Files are deleted by a helper process once this process has exited.
pub fn uninstall(remove_data: bool) -> Result<()> {
    let dir = install_dir();
    tracing::info!("Uninstalling from {}", dir.display());
    // The uninstaller usually runs from this folder itself; its exe is deleted after exit.
    stop_instances_in(&dir);

    if reg_get_string(RUN_KEY, APP_NAME).is_some_and(|cmd| {
        Path::new(cmd.trim_matches('"'))
            .parent()
            .is_some_and(|p| same_path(p, &dir))
    }) {
        set_autostart_for(&dir.join("printer-bridge.exe"), false)?;
    }
    let _ = std::fs::remove_file(start_menu_shortcut());
    reg_delete_tree(UNINSTALL_KEY);

    // Delete after exit (the uninstaller itself runs from that folder). ping is used as a
    // delay because `timeout` refuses to run without a console.
    let quoted = |p: PathBuf| format!("\"{}\"", p.display());
    let delete = if remove_data {
        format!("rd /s /q {}", quoted(dir.clone()))
    } else {
        let files: Vec<String> = PROGRAM_FILES.iter().map(|n| quoted(dir.join(n))).collect();
        let dirs: Vec<String> = PROGRAM_DIRS
            .iter()
            .map(|n| format!("rd /s /q {}", quoted(dir.join(n))))
            .collect();
        format!("del /f /q {} & {}", files.join(" "), dirs.join(" & "))
    };
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("cmd.exe")
        .raw_arg(format!("/d /c \"ping -n 3 127.0.0.1 >nul & {delete}\""))
        .current_dir(std::env::temp_dir())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("Cannot start the clean-up of the program folder")?;
    Ok(())
}

/// Start `exe` as a separate process (the newly installed copy).
pub fn launch(exe: &Path) -> Result<()> {
    std::process::Command::new(exe)
        .current_dir(exe.parent().unwrap_or(Path::new(".")))
        .spawn()
        .with_context(|| format!("Cannot start {}", exe.display()))?;
    Ok(())
}

// --- shell link ---

fn create_shortcut(link: &Path, exe: &Path, dir: &Path) -> Result<()> {
    unsafe {
        // S_FALSE / RPC_E_CHANGED_MODE are fine: COM is usable on this thread either way.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let shell_link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        shell_link.SetPath(&HSTRING::from(exe.as_os_str()))?;
        shell_link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
        shell_link.SetDescription(&HSTRING::from(APP_NAME))?;
        shell_link.SetIconLocation(&HSTRING::from(exe.as_os_str()), 0)?;
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent)?;
        }
        shell_link
            .cast::<IPersistFile>()?
            .Save(&HSTRING::from(link.as_os_str()), true)
            .context("Cannot create the Start menu shortcut")?;
    }
    Ok(())
}

// --- registry (HKEY_CURRENT_USER) ---

fn reg_open(subkey: &str) -> Result<HKEY> {
    let mut key = HKEY::default();
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(subkey),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
    };
    rc.ok()
        .with_context(|| format!("Cannot open registry key {subkey}"))?;
    Ok(key)
}

fn reg_set_string(subkey: &str, name: &str, value: &str) -> Result<()> {
    let key = reg_open(subkey)?;
    let data: Vec<u8> = value
        .encode_utf16()
        .chain(Some(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    let rc = unsafe { RegSetValueExW(key, &HSTRING::from(name), None, REG_SZ, Some(&data)) };
    unsafe {
        let _ = RegCloseKey(key);
    }
    rc.ok()
        .with_context(|| format!("Cannot write registry value {subkey}\\{name}"))
}

fn reg_set_dword(subkey: &str, name: &str, value: u32) -> Result<()> {
    let key = reg_open(subkey)?;
    let rc = unsafe {
        RegSetValueExW(
            key,
            &HSTRING::from(name),
            None,
            REG_DWORD,
            Some(&value.to_le_bytes()),
        )
    };
    unsafe {
        let _ = RegCloseKey(key);
    }
    rc.ok()
        .with_context(|| format!("Cannot write registry value {subkey}\\{name}"))
}

fn reg_get_string(subkey: &str, name: &str) -> Option<String> {
    let (subkey, name) = (HSTRING::from(subkey), HSTRING::from(name));
    let mut size = 0u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &subkey,
            &name,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
        .ok()
        .ok()?;
        let mut buf = vec![0u16; (size as usize).div_ceil(2)];
        RegGetValueW(
            HKEY_CURRENT_USER,
            &subkey,
            &name,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .ok()
        .ok()?;
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
    }
}

fn reg_delete_value(subkey: &str, name: &str) {
    if let Ok(key) = reg_open(subkey) {
        unsafe {
            let _ = RegDeleteValueW(key, &HSTRING::from(name));
            let _ = RegCloseKey(key);
        }
    }
}

fn reg_delete_tree(subkey: &str) {
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(subkey));
    }
}
