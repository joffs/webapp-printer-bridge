//! System tray icon with context menu and balloon notifications (plain Win32).
//! `run` must be called on the main thread; `notify`/`quit` may be called from anywhere.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

use crate::notify::Level;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuCommand {
    Settings,
    OpenConfig,
    Install,
    AppDirectory,
    LogDirectory,
    Restart,
    Exit,
}

const WM_TRAYICON: u32 = WM_APP + 1;
const WM_SHOW_PENDING: u32 = WM_APP + 2;
const ICON_ID: u32 = 1;

const SETTINGS_ID: u32 = 6;
const INSTALL_ID: u32 = 7;

const MENU: &[(u32, PCWSTR, Option<MenuCommand>)] = &[
    (SETTINGS_ID, w!("Settings..."), Some(MenuCommand::Settings)),
    (1, w!("Open config.json"), Some(MenuCommand::OpenConfig)),
    (0, PCWSTR::null(), None),
    (
        INSTALL_ID,
        w!("Install for this user..."),
        Some(MenuCommand::Install),
    ),
    (2, w!("App Directory"), Some(MenuCommand::AppDirectory)),
    (3, w!("Log Directory"), Some(MenuCommand::LogDirectory)),
    (0, PCWSTR::null(), None),
    (4, w!("Restart"), Some(MenuCommand::Restart)),
    (5, w!("Exit"), Some(MenuCommand::Exit)),
];

static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static PENDING: Mutex<VecDeque<(Level, String, String)>> = Mutex::new(VecDeque::new());
static HANDLER: OnceLock<Box<dyn Fn(MenuCommand) + Send + Sync>> = OnceLock::new();
static TOOLTIP: OnceLock<String> = OnceLock::new();
static OFFER_INSTALL: AtomicBool = AtomicBool::new(false);

/// Show "Install for this user..." in the menu (for copies not running from the install folder).
pub fn offer_install(offer: bool) {
    OFFER_INSTALL.store(offer, Ordering::SeqCst);
}

/// Show a balloon notification (queued until the tray icon exists).
pub fn notify(level: Level, title: &str, message: &str) {
    tracing::info!("Notification [{level:?}] {title}: {message}");
    PENDING
        .lock()
        .unwrap()
        .push_back((level, title.to_string(), message.to_string()));
    post(WM_SHOW_PENDING);
}

/// End the message loop started by `run`.
pub fn quit() {
    post(WM_CLOSE);
}

fn post(message: u32) {
    let raw = HWND_RAW.load(Ordering::SeqCst);
    if raw != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(raw as _)), message, WPARAM(0), LPARAM(0));
        }
    }
}

/// Create the tray icon and run the Win32 message loop until `quit` / Exit.
/// `handler` runs on the UI thread and must not block.
pub fn run(tooltip: &str, handler: impl Fn(MenuCommand) + Send + Sync + 'static) -> Result<()> {
    let _ = HANDLER.set(Box::new(handler));
    let _ = TOOLTIP.set(tooltip.to_string());

    unsafe {
        let instance = GetModuleHandleW(None)?;
        // Must match install::TRAY_CLASS (used to find running instances).
        let class_name = w!("WebAppPrinterBridgeTray");
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&class);
        TASKBAR_CREATED.store(
            RegisterWindowMessageW(w!("TaskbarCreated")),
            Ordering::SeqCst,
        );

        // Hidden top-level window (message-only windows miss the TaskbarCreated broadcast).
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            &HSTRING::from(crate::APP_NAME),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        HWND_RAW.store(hwnd.0 as isize, Ordering::SeqCst);
        add_icon(hwnd);
        let _ = PostMessageW(Some(hwnd), WM_SHOW_PENDING, WPARAM(0), LPARAM(0));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // Tab / Enter / Esc handling for the settings window.
            if let Some(dialog) = crate::settings::current()
                && IsDialogMessageW(dialog, &msg).as_bool()
            {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        HWND_RAW.store(0, Ordering::SeqCst);
    }
    Ok(())
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        ..Default::default()
    }
}

fn copy_wide<const N: usize>(dest: &mut [u16; N], text: &str) {
    let wide: Vec<u16> = text.encode_utf16().take(N - 1).collect();
    dest[..wide.len()].copy_from_slice(&wide);
    dest[wide.len()] = 0;
}

unsafe fn add_icon(hwnd: HWND) {
    unsafe {
        let mut data = icon_data(hwnd);
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = WM_TRAYICON;
        data.hIcon = app_icon(GetSystemMetrics(SM_CXSMICON));
        copy_wide(&mut data.szTip, TOOLTIP.get().map_or("", String::as_str));
        let _ = Shell_NotifyIconW(NIM_ADD, &data);
    }
}

/// The application icon at `size` x `size` pixels.
pub(crate) fn app_icon(size: i32) -> HICON {
    unsafe {
        let (cx, cy) = (size, size);
        // Resource id 1 is embedded by build.rs from assets/icon.ico.
        let loaded = GetModuleHandleW(None).ok().and_then(|instance| {
            LoadImageW(
                Some(instance.into()),
                PCWSTR(1 as _),
                IMAGE_ICON,
                cx,
                cy,
                LR_DEFAULTCOLOR,
            )
            .ok()
        });
        match loaded {
            Some(handle) => HICON(handle.0),
            None => LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
        }
    }
}

unsafe fn show_balloons(hwnd: HWND) {
    let pending: Vec<_> = PENDING.lock().unwrap().drain(..).collect();
    for (level, title, message) in pending {
        let mut data = icon_data(hwnd);
        data.uFlags = NIF_INFO;
        data.dwInfoFlags = match level {
            Level::Info => NIIF_INFO,
            Level::Warning => NIIF_WARNING,
            Level::Error => NIIF_ERROR,
        };
        copy_wide(&mut data.szInfoTitle, &title);
        copy_wide(
            &mut data.szInfo,
            if message.is_empty() { " " } else { &message },
        );
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }
}

unsafe fn show_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        for &(id, label, _) in MENU {
            if id == INSTALL_ID && !OFFER_INSTALL.load(Ordering::SeqCst) {
                continue;
            }
            if id == 0 {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            } else {
                let _ = AppendMenuW(menu, MF_STRING, id as usize, label);
            }
        }
        let _ = SetMenuDefaultItem(menu, SETTINGS_ID, 0);
        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);
        // Required so the menu closes when clicking elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            None,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);

        let command = MENU
            .iter()
            .find(|(id, _, _)| *id != 0 && *id as i32 == chosen.0)
            .and_then(|e| e.2);
        if let (Some(command), Some(handler)) = (command, HANDLER.get()) {
            handler(command);
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_TRAYICON => {
                let event = lparam.0 as u32;
                if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                    show_menu(hwnd);
                } else if event == WM_LBUTTONUP
                    && let Some(handler) = HANDLER.get()
                {
                    handler(MenuCommand::Settings);
                }
                LRESULT(0)
            }
            WM_SHOW_PENDING => {
                show_balloons(hwnd);
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd));
                PostQuitMessage(0);
                LRESULT(0)
            }
            m if m != 0 && m == TASKBAR_CREATED.load(Ordering::SeqCst) => {
                // Explorer restarted: the icon has to be added again.
                add_icon(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Open a file or folder with its default handler. Files without an association
/// (e.g. `.json` on a fresh Windows) are opened in Notepad.
pub fn shell_open(path: &Path) {
    let target = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let target = HSTRING::from(target.as_os_str());
    unsafe {
        let result = ShellExecuteW(
            None,
            w!("open"),
            &target,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        if result.0 as isize <= 32 && path.is_file() {
            ShellExecuteW(
                None,
                w!("open"),
                w!("notepad.exe"),
                &target,
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
    }
}
