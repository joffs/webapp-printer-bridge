//! Settings window: printer aliases (alias -> installed printer + options), handling of
//! unknown aliases, server bind/port/token, notifications, log file and autostart. Runs on
//! the tray's UI thread.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::net::Ipv4Addr;

use anyhow::Result;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    COLOR_BTNFACE, CreateFontIndirectW, DeleteObject, HBRUSH, HFONT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForWindow, SystemParametersInfoForDpi,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PWSTR, w};

use crate::config::{self, Config, PrinterMapping};
use crate::netinfo;

/// Receives the edited config and the "Start with Windows" state.
type SaveFn = Box<dyn Fn(Config, bool) -> Result<()>>;

// Control ids. IDOK/IDCANCEL (1/2) are Save/Cancel so Enter/Esc work.
const ID_SAVE: i32 = 1;
const ID_CANCEL: i32 = 2;
const ID_LIST: i32 = 100;
const ID_ADD: i32 = 101;
const ID_REMOVE: i32 = 102;
const ID_ALIAS: i32 = 103;
const ID_PRINTER: i32 = 104;
const ID_ROTATE: i32 = 105;
const ID_RESET: i32 = 106;
const ID_DPI: i32 = 107;
const ID_AUTOADD: i32 = 110;
const ID_FALLBACK: i32 = 111;
const ID_BIND: i32 = 120;
const ID_PORT: i32 = 121;
const ID_TOKEN_ON: i32 = 122;
const ID_TOKEN: i32 = 123;
const ID_NOTIFY: i32 = 124;
const ID_BIND_HINT: i32 = 125;
const ID_LOG: i32 = 126;
const ID_AUTOSTART: i32 = 127;

const CLIENT_W: i32 = 600;
const CLIENT_H: i32 = 704;
const LIST_W: i32 = 556;
const ALIAS_COL_W: i32 = 180;

#[derive(Clone, Copy)]
enum Kind {
    Group,
    Label,
    Edit,
    NumberEdit,
    DropList,
    DropDown,
    Check,
    Button,
    DefaultButton,
    List,
}

/// (id, kind, text, x, y, w, h) in 96-DPI units; creation order is the Tab order.
const LAYOUT: &[(i32, Kind, &str, i32, i32, i32, i32)] = &[
    (200, Kind::Group, "Printer aliases", 10, 8, 580, 372),
    (ID_LIST, Kind::List, "", 22, 30, LIST_W, 140),
    (ID_ADD, Kind::Button, "&Add", 22, 178, 90, 26),
    (ID_REMOVE, Kind::Button, "&Remove", 118, 178, 90, 26),
    (201, Kind::Label, "Alias:", 22, 218, 90, 20),
    (ID_ALIAS, Kind::Edit, "", 118, 214, 240, 24),
    (202, Kind::Label, "Printer:", 22, 252, 90, 20),
    (ID_PRINTER, Kind::DropList, "", 118, 248, 460, 300),
    (
        ID_ROTATE,
        Kind::Check,
        "Auto rotate: print PDF pages that are wider than tall in landscape",
        118,
        284,
        460,
        22,
    ),
    (
        ID_RESET,
        Kind::Check,
        "Use the whole sheet (ignore the printer's margins)",
        118,
        310,
        460,
        22,
    ),
    (203, Kind::Label, "Force DPI:", 22, 344, 90, 20),
    (ID_DPI, Kind::NumberEdit, "", 118, 340, 70, 24),
    (
        204,
        Kind::Label,
        "0 = vector output (recommended)",
        198,
        344,
        300,
        20,
    ),
    (205, Kind::Group, "Unknown aliases", 10, 388, 580, 78),
    (
        ID_AUTOADD,
        Kind::Check,
        "Add unknown aliases to this list automatically (printer to be assigned)",
        22,
        410,
        556,
        22,
    ),
    (
        ID_FALLBACK,
        Kind::Check,
        "Print unknown aliases on the Windows default printer",
        22,
        434,
        556,
        22,
    ),
    (206, Kind::Group, "Server and app", 10, 474, 580, 178),
    (207, Kind::Label, "Bind address:", 22, 500, 94, 20),
    (ID_BIND, Kind::DropDown, "", 118, 496, 260, 300),
    (208, Kind::Label, "Port:", 392, 500, 40, 20),
    (ID_PORT, Kind::NumberEdit, "", 436, 496, 80, 24),
    (ID_BIND_HINT, Kind::Label, "", 118, 524, 460, 20),
    (ID_TOKEN_ON, Kind::Check, "Require token:", 22, 548, 110, 22),
    (ID_TOKEN, Kind::Edit, "", 136, 547, 242, 24),
    (
        ID_NOTIFY,
        Kind::Check,
        "Show notifications for print jobs",
        22,
        576,
        556,
        22,
    ),
    (
        ID_LOG,
        Kind::Check,
        "Write log file (log\\client.log, for troubleshooting)",
        22,
        600,
        556,
        22,
    ),
    (
        ID_AUTOSTART,
        Kind::Check,
        "Start with Windows (when you sign in)",
        22,
        624,
        556,
        22,
    ),
    (ID_SAVE, Kind::DefaultButton, "Save", 404, 664, 90, 28),
    (ID_CANCEL, Kind::Button, "Cancel", 500, 664, 90, 28),
];

struct Settings {
    hwnd: HWND,
    controls: HashMap<i32, HWND>,
    font: HFONT,
    dpi: u32,
    config: Config,
    printers: Vec<String>,
    local_ips: Vec<(Ipv4Addr, String)>,
    /// Printer names behind the entries of the printer drop-down ("" = not assigned).
    printer_items: Vec<String>,
    selected: Option<usize>,
    /// "Start with Windows" (registry, not config.json).
    autostart: bool,
    on_save: SaveFn,
}

thread_local! {
    static STATE: RefCell<Option<Settings>> = const { RefCell::new(None) };
    static WINDOW: Cell<isize> = const { Cell::new(0) };
}

/// The open settings window, for `IsDialogMessage` in the message loop.
pub fn current() -> Option<HWND> {
    let raw = WINDOW.with(Cell::get);
    (raw != 0).then_some(HWND(raw as _))
}

/// Run `f` on the window state. Returns `None` while the state is already borrowed, which
/// happens exactly when a control notification is caused by our own programmatic update.
fn with_state<R>(f: impl FnOnce(&mut Settings) -> R) -> Option<R> {
    STATE.with(|cell| {
        cell.try_borrow_mut()
            .ok()
            .and_then(|mut guard| guard.as_mut().map(f))
    })
}

/// Open the settings window (or bring the open one to the front). `on_save` receives the
/// edited config and the "Start with Windows" state; an error keeps the window open and is
/// shown to the user.
pub fn open(
    config: Config,
    printers: Vec<String>,
    autostart: bool,
    on_save: impl Fn(Config, bool) -> Result<()> + 'static,
) {
    if let Some(hwnd) = current() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
        return;
    }
    if let Err(e) = unsafe { create(config, printers, autostart, Box::new(on_save)) } {
        tracing::error!("Cannot open settings window: {e:#}");
    }
}

unsafe fn create(
    config: Config,
    printers: Vec<String>,
    autostart: bool,
    on_save: SaveFn,
) -> Result<()> {
    unsafe {
        let init = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES,
        };
        let _ = InitCommonControlsEx(&init);

        let instance = GetModuleHandleW(None)?;
        let class_name = w!("WebAppPrinterBridgeSettings");
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: class_name,
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as _),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: crate::tray::app_icon(32),
            ..Default::default()
        };
        RegisterClassW(&class);

        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let hwnd = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            class_name,
            &HSTRING::from(format!("{} - Settings", crate::APP_NAME)),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            100,
            100,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        let dpi = GetDpiForWindow(hwnd).max(96);

        let mut controls = HashMap::new();
        for &(id, kind, text, ..) in LAYOUT {
            let (class, style, ex) = match kind {
                Kind::Group => (w!("BUTTON"), BS_GROUPBOX, WINDOW_EX_STYLE(0)),
                Kind::Label => (w!("STATIC"), 0, WINDOW_EX_STYLE(0)),
                Kind::Edit => (
                    w!("EDIT"),
                    ES_AUTOHSCROLL | WS_TABSTOP.0 as i32,
                    WS_EX_CLIENTEDGE,
                ),
                Kind::NumberEdit => (
                    w!("EDIT"),
                    ES_AUTOHSCROLL | ES_NUMBER | WS_TABSTOP.0 as i32,
                    WS_EX_CLIENTEDGE,
                ),
                Kind::DropList => (
                    w!("COMBOBOX"),
                    CBS_DROPDOWNLIST | WS_VSCROLL.0 as i32 | WS_TABSTOP.0 as i32,
                    WINDOW_EX_STYLE(0),
                ),
                Kind::DropDown => (
                    w!("COMBOBOX"),
                    CBS_DROPDOWN | CBS_AUTOHSCROLL | WS_VSCROLL.0 as i32 | WS_TABSTOP.0 as i32,
                    WINDOW_EX_STYLE(0),
                ),
                Kind::Check => (
                    w!("BUTTON"),
                    BS_AUTOCHECKBOX | WS_TABSTOP.0 as i32,
                    WINDOW_EX_STYLE(0),
                ),
                Kind::Button => (
                    w!("BUTTON"),
                    BS_PUSHBUTTON | WS_TABSTOP.0 as i32,
                    WINDOW_EX_STYLE(0),
                ),
                Kind::DefaultButton => (
                    w!("BUTTON"),
                    BS_DEFPUSHBUTTON | WS_TABSTOP.0 as i32,
                    WINDOW_EX_STYLE(0),
                ),
                Kind::List => (
                    WC_LISTVIEWW,
                    (LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS | LVS_NOSORTHEADER) as i32
                        | WS_TABSTOP.0 as i32,
                    WS_EX_CLIENTEDGE,
                ),
            };
            let control = CreateWindowExW(
                ex,
                class,
                &HSTRING::from(text),
                WINDOW_STYLE(style as u32) | WS_CHILD | WS_VISIBLE,
                0,
                0,
                0,
                0,
                Some(hwnd),
                Some(HMENU(id as isize as _)),
                Some(instance.into()),
                None,
            )?;
            controls.insert(id, control);
        }

        let list = controls[&ID_LIST];
        let ex = (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as usize;
        SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            Some(WPARAM(ex)),
            Some(LPARAM(ex as isize)),
        );
        for (index, title) in ["Alias", "Printer"].iter().enumerate() {
            let mut text: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
            let column = LVCOLUMNW {
                mask: LVCF_TEXT | LVCF_WIDTH,
                cx: 100,
                pszText: PWSTR(text.as_mut_ptr()),
                ..Default::default()
            };
            SendMessageW(
                list,
                LVM_INSERTCOLUMNW,
                Some(WPARAM(index)),
                Some(LPARAM(&column as *const _ as isize)),
            );
        }

        let settings = Settings {
            hwnd,
            controls,
            font: HFONT::default(),
            dpi,
            local_ips: netinfo::local_ipv4(),
            printers,
            config,
            printer_items: Vec::new(),
            selected: None,
            autostart,
            on_save,
        };
        STATE.with(|cell| *cell.borrow_mut() = Some(settings));
        WINDOW.with(|w| w.set(hwnd.0 as isize));

        with_state(|s| {
            s.apply_dpi(dpi, None);
            s.fill_globals();
            s.refresh_list(if s.config.printer.mappings.is_empty() {
                None
            } else {
                Some(0)
            });
        });

        // Center on the primary screen.
        let mut rect = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rect);
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        let x = (GetSystemMetrics(SM_CXSCREEN) - w) / 2;
        let y = (GetSystemMetrics(SM_CYSCREEN) - h) / 2;
        let _ = SetWindowPos(
            hwnd,
            None,
            x.max(0),
            y.max(0),
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER,
        );
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        Ok(())
    }
}

impl Settings {
    fn ctl(&self, id: i32) -> HWND {
        self.controls[&id]
    }

    fn scale(&self, v: i32) -> i32 {
        (v as i64 * self.dpi as i64 / 96) as i32
    }

    /// Size the window, create the font and position all controls for `dpi`.
    unsafe fn apply_dpi(&mut self, dpi: u32, suggested: Option<&RECT>) {
        unsafe {
            self.dpi = dpi;
            match suggested {
                Some(r) => {
                    let _ = SetWindowPos(
                        self.hwnd,
                        None,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                None => {
                    let mut rect = RECT {
                        left: 0,
                        top: 0,
                        right: self.scale(CLIENT_W),
                        bottom: self.scale(CLIENT_H),
                    };
                    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
                    let _ =
                        AdjustWindowRectExForDpi(&mut rect, style, false, WS_EX_CONTROLPARENT, dpi);
                    let _ = SetWindowPos(
                        self.hwnd,
                        None,
                        0,
                        0,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
            }

            let mut metrics = NONCLIENTMETRICSW {
                cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
                ..Default::default()
            };
            let _ = SystemParametersInfoForDpi(
                SPI_GETNONCLIENTMETRICS.0,
                metrics.cbSize,
                Some(&mut metrics as *mut _ as *mut _),
                0,
                dpi,
            );
            let font = CreateFontIndirectW(&metrics.lfMessageFont);
            if !self.font.is_invalid() {
                let _ = DeleteObject(self.font.into());
            }
            self.font = font;

            for &(id, _, _, x, y, w, h) in LAYOUT {
                let control = self.ctl(id);
                let _ = MoveWindow(
                    control,
                    self.scale(x),
                    self.scale(y),
                    self.scale(w),
                    self.scale(h),
                    true,
                );
                SendMessageW(
                    control,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
            let list = self.ctl(ID_LIST);
            let alias_w = self.scale(ALIAS_COL_W);
            let printer_w =
                self.scale(LIST_W) - alias_w - GetSystemMetrics(SM_CXVSCROLL) - self.scale(6);
            SendMessageW(
                list,
                LVM_SETCOLUMNWIDTH,
                Some(WPARAM(0)),
                Some(LPARAM(alias_w as isize)),
            );
            SendMessageW(
                list,
                LVM_SETCOLUMNWIDTH,
                Some(WPARAM(1)),
                Some(LPARAM(printer_w as isize)),
            );
        }
    }

    unsafe fn fill_globals(&mut self) {
        unsafe {
            let c = self.config.clone();
            set_check(self.ctl(ID_AUTOADD), c.printer.auto_add_unknown_type);
            set_check(self.ctl(ID_FALLBACK), c.printer.fallback_to_default);
            set_check(self.ctl(ID_NOTIFY), c.gui.notification.enabled);
            set_check(self.ctl(ID_LOG), c.log.enabled);
            set_check(self.ctl(ID_AUTOSTART), self.autostart);
            set_check(self.ctl(ID_TOKEN_ON), c.server.authentication.enabled);
            set_text(
                self.ctl(ID_TOKEN),
                c.server.authentication.token.as_deref().unwrap_or(""),
            );
            set_text(self.ctl(ID_PORT), &c.server.port.to_string());
            let _ = EnableWindow(self.ctl(ID_TOKEN), c.server.authentication.enabled);

            let bind = self.ctl(ID_BIND);
            let mut items = vec![
                "0.0.0.0   (all network adapters)".to_string(),
                "127.0.0.1   (this PC only)".to_string(),
            ];
            items.extend(
                self.local_ips
                    .iter()
                    .map(|(ip, name)| format!("{ip}   ({name})")),
            );
            for item in &items {
                SendMessageW(
                    bind,
                    CB_ADDSTRING,
                    None,
                    Some(LPARAM(HSTRING::from(item.as_str()).as_ptr() as isize)),
                );
            }
            set_text(bind, &c.server.bind);
            self.update_bind_hint(&c.server.bind);
        }
    }

    fn printer_label(&self, name: &str) -> String {
        if name.is_empty() {
            "\u{26A0} not assigned".into()
        } else if let Some(installed) = self.printers.iter().find(|p| p.eq_ignore_ascii_case(name))
        {
            installed.clone()
        } else {
            format!("\u{26A0} {name} (not installed)")
        }
    }

    unsafe fn refresh_list(&mut self, select: Option<usize>) {
        unsafe {
            let list = self.ctl(ID_LIST);
            SendMessageW(list, LVM_DELETEALLITEMS, None, None);
            for (i, mapping) in self.config.printer.mappings.iter().enumerate() {
                let mut alias: Vec<u16> = mapping.kind.encode_utf16().chain(Some(0)).collect();
                let item = LVITEMW {
                    mask: LVIF_TEXT,
                    iItem: i as i32,
                    pszText: PWSTR(alias.as_mut_ptr()),
                    ..Default::default()
                };
                SendMessageW(
                    list,
                    LVM_INSERTITEMW,
                    None,
                    Some(LPARAM(&item as *const _ as isize)),
                );
                self.set_printer_cell(i);
            }
            self.select(select);
        }
    }

    unsafe fn set_list_text(&self, row: usize, column: i32, text: &str) {
        let mut wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let item = LVITEMW {
            iSubItem: column,
            pszText: PWSTR(wide.as_mut_ptr()),
            ..Default::default()
        };
        unsafe {
            SendMessageW(
                self.ctl(ID_LIST),
                LVM_SETITEMTEXTW,
                Some(WPARAM(row)),
                Some(LPARAM(&item as *const _ as isize)),
            );
        }
    }

    unsafe fn set_printer_cell(&self, row: usize) {
        let label = self.printer_label(&self.config.printer.mappings[row].name);
        unsafe { self.set_list_text(row, 1, &label) };
    }

    unsafe fn select(&mut self, row: Option<usize>) {
        unsafe {
            let list = self.ctl(ID_LIST);
            let row = row.filter(|&r| r < self.config.printer.mappings.len());
            if let Some(row) = row {
                let state = LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0);
                let item = LVITEMW {
                    state,
                    stateMask: state,
                    ..Default::default()
                };
                SendMessageW(
                    list,
                    LVM_SETITEMSTATE,
                    Some(WPARAM(row)),
                    Some(LPARAM(&item as *const _ as isize)),
                );
                SendMessageW(list, LVM_ENSUREVISIBLE, Some(WPARAM(row)), Some(LPARAM(0)));
            }
            self.show_details(row);
        }
    }

    /// Load the selected alias into the detail controls (or clear and disable them).
    unsafe fn show_details(&mut self, row: Option<usize>) {
        unsafe {
            self.selected = row;
            let mapping = row.map(|r| self.config.printer.mappings[r].clone());
            for id in [ID_ALIAS, ID_PRINTER, ID_ROTATE, ID_RESET, ID_DPI, ID_REMOVE] {
                let _ = EnableWindow(self.ctl(id), mapping.is_some());
            }
            let m = mapping.unwrap_or_default();
            set_text(self.ctl(ID_ALIAS), &m.kind);
            set_check(self.ctl(ID_ROTATE), m.auto_rotate);
            set_check(self.ctl(ID_RESET), m.reset_imageable_area);
            set_text(self.ctl(ID_DPI), &m.force_dpi.to_string());

            // Printer drop-down: "not assigned", installed printers, and the configured
            // printer if it is not installed (so its name is not lost silently).
            let combo = self.ctl(ID_PRINTER);
            SendMessageW(combo, CB_RESETCONTENT, None, None);
            self.printer_items = std::iter::once(String::new())
                .chain(self.printers.iter().cloned())
                .collect();
            if !m.name.is_empty()
                && !self
                    .printers
                    .iter()
                    .any(|p| p.eq_ignore_ascii_case(&m.name))
            {
                self.printer_items.push(m.name.clone());
            }
            for name in &self.printer_items {
                let label = if name.is_empty() || !self.printers.contains(name) {
                    self.printer_label(name)
                } else {
                    name.clone()
                };
                SendMessageW(
                    combo,
                    CB_ADDSTRING,
                    None,
                    Some(LPARAM(HSTRING::from(label).as_ptr() as isize)),
                );
            }
            let current = self
                .printer_items
                .iter()
                .position(|p| p.eq_ignore_ascii_case(&m.name))
                .unwrap_or(0);
            SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(current)), None);
        }
    }

    fn mapping(&mut self) -> Option<&mut PrinterMapping> {
        self.selected
            .and_then(|r| self.config.printer.mappings.get_mut(r))
    }

    unsafe fn on_command(&mut self, id: i32, code: u32) -> Option<Action> {
        unsafe {
            match (id, code) {
                (ID_ADD, BN_CLICKED) => {
                    let mut kind = "NEW_ALIAS".to_string();
                    let mut n = 2;
                    while self.config.printer.mappings.iter().any(|m| m.kind == kind) {
                        kind = format!("NEW_ALIAS{n}");
                        n += 1;
                    }
                    self.config.printer.mappings.push(PrinterMapping {
                        kind,
                        ..Default::default()
                    });
                    let row = self.config.printer.mappings.len() - 1;
                    self.refresh_list(Some(row));
                    let alias = self.ctl(ID_ALIAS);
                    let _ = SetFocus(Some(alias));
                    SendMessageW(alias, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
                }
                (ID_REMOVE, BN_CLICKED) => {
                    if let Some(row) = self.selected {
                        self.config.printer.mappings.remove(row);
                        let next = row.min(self.config.printer.mappings.len().saturating_sub(1));
                        let next = (!self.config.printer.mappings.is_empty()).then_some(next);
                        self.refresh_list(next);
                    }
                }
                (ID_ALIAS, EN_CHANGE) => {
                    let text = get_text(self.ctl(ID_ALIAS)).trim().to_string();
                    if let (Some(row), Some(m)) = (self.selected, self.mapping()) {
                        m.kind = text.clone();
                        self.set_list_text(row, 0, &text);
                    }
                }
                (ID_PRINTER, CBN_SELCHANGE) => {
                    let index = SendMessageW(self.ctl(ID_PRINTER), CB_GETCURSEL, None, None).0;
                    if let Some(name) = usize::try_from(index)
                        .ok()
                        .and_then(|i| self.printer_items.get(i))
                        .cloned()
                        && let Some(row) = self.selected
                    {
                        self.config.printer.mappings[row].name = name;
                        self.set_printer_cell(row);
                    }
                }
                (ID_ROTATE, BN_CLICKED) => {
                    let checked = get_check(self.ctl(ID_ROTATE));
                    if let Some(m) = self.mapping() {
                        m.auto_rotate = checked;
                    }
                }
                (ID_RESET, BN_CLICKED) => {
                    let checked = get_check(self.ctl(ID_RESET));
                    if let Some(m) = self.mapping() {
                        m.reset_imageable_area = checked;
                    }
                }
                (ID_DPI, EN_CHANGE) => {
                    let dpi = get_text(self.ctl(ID_DPI)).trim().parse().unwrap_or(0);
                    if let Some(m) = self.mapping() {
                        m.force_dpi = dpi;
                    }
                }
                (ID_TOKEN_ON, BN_CLICKED) => {
                    let _ = EnableWindow(self.ctl(ID_TOKEN), get_check(self.ctl(ID_TOKEN_ON)));
                }
                (ID_BIND, CBN_EDITCHANGE) => {
                    let text = get_text(self.ctl(ID_BIND));
                    self.update_bind_hint(&text);
                }
                (ID_BIND, CBN_SELCHANGE) => {
                    let combo = self.ctl(ID_BIND);
                    let index = SendMessageW(combo, CB_GETCURSEL, None, None).0;
                    if index >= 0 {
                        let len = SendMessageW(
                            combo,
                            CB_GETLBTEXTLEN,
                            Some(WPARAM(index as usize)),
                            None,
                        )
                        .0;
                        let mut buf = vec![0u16; len.max(0) as usize + 1];
                        SendMessageW(
                            combo,
                            CB_GETLBTEXT,
                            Some(WPARAM(index as usize)),
                            Some(LPARAM(buf.as_mut_ptr() as isize)),
                        );
                        let text = String::from_utf16_lossy(&buf[..len.max(0) as usize]);
                        self.update_bind_hint(&text);
                    }
                }
                (ID_SAVE, _) => return self.save(),
                (ID_CANCEL, _) => return Some(Action::Close),
                _ => {}
            }
            None
        }
    }

    unsafe fn update_bind_hint(&self, text: &str) {
        let bind = first_token(text);
        let hint = match bind.parse::<Ipv4Addr>() {
            _ if bind.is_empty() => "\u{26A0} Enter an address to listen on.".to_string(),
            Ok(ip) if ip.is_unspecified() => "Reachable from other PCs on the network.".into(),
            Ok(ip) if ip.is_loopback() => "Only reachable from browsers on this PC.".into(),
            _ if !netinfo::is_bindable(bind, &self.local_ips) => {
                format!("\u{26A0} {bind} is not an address of this PC - the server cannot start.")
            }
            _ => "Reachable from other PCs via this address.".into(),
        };
        unsafe { set_text(self.ctl(ID_BIND_HINT), &hint) };
    }

    unsafe fn save(&mut self) -> Option<Action> {
        unsafe {
            let mut config = self.config.clone();
            config.printer.auto_add_unknown_type = get_check(self.ctl(ID_AUTOADD));
            config.printer.fallback_to_default = get_check(self.ctl(ID_FALLBACK));
            config.gui.notification.enabled = get_check(self.ctl(ID_NOTIFY));
            config.log.enabled = get_check(self.ctl(ID_LOG));
            config.server.bind = first_token(&get_text(self.ctl(ID_BIND))).to_string();
            config.server.port = get_text(self.ctl(ID_PORT)).trim().parse().unwrap_or(0);
            config.server.authentication.enabled = get_check(self.ctl(ID_TOKEN_ON));
            let token = get_text(self.ctl(ID_TOKEN));
            config.server.authentication.token = (!token.is_empty()).then_some(token);

            let problems = config::validate(&config);
            if !problems.is_empty() {
                message(self.hwnd, &problems.join("\n"), MB_ICONWARNING | MB_OK);
                return None;
            }
            if !netinfo::is_bindable(&config.server.bind, &self.local_ips) {
                let text = format!(
                    "{} is not an address of this PC, so the server will not start.\n\nSave anyway?",
                    config.server.bind
                );
                if message(self.hwnd, &text, MB_ICONWARNING | MB_YESNO) != IDYES {
                    return None;
                }
            }
            match (self.on_save)(config, get_check(self.ctl(ID_AUTOSTART))) {
                Ok(()) => Some(Action::Close),
                Err(e) => {
                    message(
                        self.hwnd,
                        &format!("Saving failed:\n{e:#}"),
                        MB_ICONERROR | MB_OK,
                    );
                    None
                }
            }
        }
    }
}

enum Action {
    Close,
}

fn first_token(text: &str) -> &str {
    text.split_whitespace().next().unwrap_or("")
}

unsafe fn message(owner: HWND, text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    unsafe {
        MessageBoxW(
            Some(owner),
            &HSTRING::from(text),
            &HSTRING::from(crate::APP_NAME),
            style,
        )
    }
}

unsafe fn set_text(hwnd: HWND, text: &str) {
    unsafe {
        let _ = SetWindowTextW(hwnd, &HSTRING::from(text));
    }
}

unsafe fn get_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd).max(0) as usize;
        let mut buf = vec![0u16; len + 1];
        let n = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
        String::from_utf16_lossy(&buf[..n])
    }
}

unsafe fn set_check(hwnd: HWND, checked: bool) {
    unsafe {
        SendMessageW(
            hwnd,
            BM_SETCHECK,
            Some(WPARAM(if checked { BST_CHECKED.0 as usize } else { 0 })),
            None,
        );
    }
}

unsafe fn get_check(hwnd: HWND) -> bool {
    unsafe { SendMessageW(hwnd, BM_GETCHECK, None, None).0 as u32 == BST_CHECKED.0 }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                let code = ((wparam.0 >> 16) & 0xFFFF) as u32;
                if let Some(Some(Action::Close)) = with_state(|s| s.on_command(id, code)) {
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_NOTIFY => {
                let header = &*(lparam.0 as *const NMHDR);
                if header.idFrom == ID_LIST as usize && header.code == LVN_ITEMCHANGED {
                    let change = &*(lparam.0 as *const NMLISTVIEW);
                    if change.uChanged.0 & LVIF_STATE.0 != 0 {
                        with_state(|s| {
                            let row = SendMessageW(
                                s.ctl(ID_LIST),
                                LVM_GETNEXTITEM,
                                Some(WPARAM(usize::MAX)),
                                Some(LPARAM(LVNI_SELECTED as isize)),
                            )
                            .0;
                            let row = usize::try_from(row).ok();
                            if row != s.selected {
                                s.show_details(row);
                            }
                        });
                    }
                }
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let dpi = (wparam.0 & 0xFFFF) as u32;
                let suggested = *(lparam.0 as *const RECT);
                with_state(|s| s.apply_dpi(dpi, Some(&suggested)));
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                WINDOW.with(|w| w.set(0));
                if let Some(state) = STATE.with(|cell| cell.borrow_mut().take())
                    && !state.font.is_invalid()
                {
                    let _ = DeleteObject(state.font.into());
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
