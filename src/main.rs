#![windows_subsystem = "windows"]

use std::path::Path;
use std::sync::Arc;

use printer_bridge::bridge::AppState;
use printer_bridge::notify::{self, Level};
use printer_bridge::server::ServerController;
use printer_bridge::tray::{self, MenuCommand};
use printer_bridge::{APP_NAME, VERSION, config, install, logging, printing, settings};
use windows::Win32::UI::WindowsAndMessaging::{
    IDNO, IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_YESNO,
    MB_YESNOCANCEL, MESSAGEBOX_RESULT, MESSAGEBOX_STYLE, MessageBoxW,
};
use windows::core::HSTRING;

fn main() {
    // config.json, log/ and downloads/ live next to the executable.
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        let _ = std::env::set_current_dir(dir);
    }
    logging::init(Path::new("log"));
    tracing::info!("Starting {APP_NAME} {VERSION}");

    if std::env::args().any(|a| a == "--uninstall") {
        uninstall();
        return;
    }
    // Unattended installation (deployment scripts): no questions, errors in a message box.
    if std::env::args().any(|a| a == "--install") {
        if let Err(e) = install::install().and_then(|exe| install::launch(&exe)) {
            tracing::error!("Installation failed: {e:#}");
            message(
                &format!("Installation failed:\n{e:#}"),
                MB_OK | MB_ICONERROR,
            );
        }
        return;
    }
    // A newer (or other) copy started while the installed one runs: offer to update it.
    if install::installed_instance_running() {
        offer_update();
        return;
    }

    notify::set_sink(tray::notify);
    check_pdfium();

    let config_path = config::config_path();
    let first_start = !config_path.exists();
    let (config, config_ok) = match config::load(&config_path) {
        Ok(config) => (config, true),
        Err(e) => {
            tracing::error!("{e:#}");
            tray::notify(
                Level::Error,
                "Invalid config.json",
                &format!("{e:#}\nFix the file, then choose Restart from the tray menu."),
            );
            (config::Config::default(), false)
        }
    };
    logging::set_file_enabled(config.log.enabled);

    let state = AppState::new(config, config_path.clone());
    let server = match ServerController::new(state.clone()) {
        Ok(server) => Arc::new(server),
        Err(e) => {
            tracing::error!("Cannot create async runtime: {e:#}");
            std::process::exit(1);
        }
    };

    if config_ok {
        start_server(&server);
    }
    tray::notify(Level::Info, APP_NAME, "is running in background!");
    let installed = install::is_installed_copy();
    tray::offer_install(!installed);
    if first_start && !installed {
        tray::notify(
            Level::Info,
            "Not installed yet",
            "Choose \"Install for this user...\" in the tray menu to install it and start it with Windows.",
        );
    }

    let ui_server = server.clone();
    let result = tray::run(APP_NAME, move |command| match command {
        MenuCommand::Settings => open_settings(&state, &ui_server),
        MenuCommand::Install => install_from_tray(&ui_server),
        MenuCommand::OpenConfig => tray::shell_open(&config_path),
        MenuCommand::AppDirectory => tray::shell_open(Path::new(".")),
        MenuCommand::LogDirectory => tray::shell_open(Path::new("log")),
        MenuCommand::Restart => {
            let (state, server) = (state.clone(), ui_server.clone());
            std::thread::spawn(move || restart(&state, &server));
        }
        MenuCommand::Exit => tray::quit(),
    });
    if let Err(e) = result {
        tracing::error!("Tray icon failed: {e:#}");
    }

    server.stop();
    tracing::info!("Exited");
}

fn message(text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    unsafe { MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(APP_NAME), style) }
}

/// Install this copy for the current user, start the installed copy and quit.
fn install_from_tray(server: &ServerController) {
    let dir = install::install_dir();
    let question = match install::installed_version() {
        Some(old) => format!(
            "Update the installed {APP_NAME} {old} to {VERSION}?\n\n\
             Its settings are kept. The installed copy is started afterwards; this copy closes."
        ),
        None => format!(
            "Install {APP_NAME} {VERSION} for this user?\n\n\
             - Program folder: {}\n\
             - Start menu entry and an entry in \"Apps & features\" (for uninstalling)\n\
             - Starts automatically when you sign in (can be turned off in Settings)\n\
             - config.json from this folder is taken over\n\n\
             The installed copy is started afterwards; this copy closes.",
            dir.display()
        ),
    };
    if message(&question, MB_YESNO | MB_ICONQUESTION) != IDYES {
        return;
    }
    // Free the port for the installed copy.
    server.stop();
    match install::install().and_then(|exe| install::launch(&exe)) {
        Ok(()) => tray::quit(),
        Err(e) => {
            tracing::error!("Installation failed: {e:#}");
            message(
                &format!("Installation failed:\n{e:#}"),
                MB_OK | MB_ICONERROR,
            );
            start_server(server);
        }
    }
}

/// Started while the installed copy is running: replace it with this version.
fn offer_update() {
    let installed = install::installed_version().unwrap_or_else(|| "?".into());
    let question = if installed == VERSION {
        format!(
            "{APP_NAME} {VERSION} is already installed and running.\n\n\
             Replace the installed copy with this one? Its settings are kept."
        )
    } else {
        format!(
            "{APP_NAME} {installed} is already running (installed for this user).\n\n\
             Update it to this version ({VERSION})? Its settings are kept."
        )
    };
    if message(&question, MB_YESNO | MB_ICONQUESTION) != IDYES {
        return;
    }
    if let Err(e) = install::install().and_then(|exe| install::launch(&exe)) {
        tracing::error!("Update failed: {e:#}");
        message(&format!("Update failed:\n{e:#}"), MB_OK | MB_ICONERROR);
    }
}

/// `printer-bridge.exe --uninstall` (from "Apps & features").
fn uninstall() {
    if message(
        &format!("Remove {APP_NAME} from this PC?"),
        MB_YESNO | MB_ICONQUESTION,
    ) != IDYES
    {
        return;
    }
    let remove_data = match message(
        "Also delete the settings (config.json), logs and downloaded files?\n\n\
         Yes: remove everything\nNo: keep the settings for a later installation",
        MB_YESNOCANCEL | MB_ICONQUESTION,
    ) {
        IDYES => true,
        IDNO => false,
        _ => return,
    };
    match install::uninstall(remove_data) {
        Ok(()) => {
            message(
                &format!("{APP_NAME} has been removed."),
                MB_OK | MB_ICONINFORMATION,
            );
        }
        Err(e) => {
            message(&format!("Uninstall failed:\n{e:#}"), MB_OK | MB_ICONERROR);
        }
    }
}

/// Refuse to start with a `pdfium.dll` of the wrong architecture (an x64 DLL next to the ARM64
/// exe or vice versa); a missing or unloadable DLL only disables PDF printing.
fn check_pdfium() {
    let dll = printing::pdfium::library_path();
    if let Err(e) = printing::pdfium::check_architecture(&dll) {
        let text = format!("{e:#}\n\nInstall the pdfium.dll that belongs to this build.");
        tracing::error!("{text}");
        message(&text, MB_OK | MB_ICONERROR);
        std::process::exit(1);
    }
    match printing::pdfium::get() {
        Ok(_) => tracing::info!("PDFium loaded from {}", dll.display()),
        Err(e) => {
            tracing::error!("{e:#}");
            tray::notify(Level::Error, "PDF printing unavailable", &format!("{e:#}"));
        }
    }
}

/// Show the settings window with the config as it is on disk (falling back to the active
/// one if the file is broken). Saving writes config.json and restarts the server.
fn open_settings(state: &Arc<AppState>, server: &Arc<ServerController>) {
    let config = config::load(state.config_path()).unwrap_or_else(|e| {
        tracing::error!("{e:#}");
        tray::notify(
            Level::Warning,
            "config.json is invalid",
            "Showing the active settings; saving will repair the file.",
        );
        (*state.config()).clone()
    });
    let printers = printing::list_printers().unwrap_or_else(|e| {
        tracing::error!("Cannot list printers: {e:#}");
        Vec::new()
    });
    let (state, server) = (state.clone(), server.clone());
    let autostart = install::autostart_enabled();
    settings::open(
        config,
        printers,
        autostart,
        move |config, start_with_windows| {
            config::save_settings(state.config_path(), &config)?;
            if start_with_windows != install::autostart_enabled() {
                install::set_autostart(start_with_windows)?;
            }
            tracing::info!("Settings saved");
            let (state, server) = (state.clone(), server.clone());
            std::thread::spawn(move || restart(&state, &server));
            Ok(())
        },
    );
}

fn start_server(server: &ServerController) -> bool {
    match server.start() {
        Ok(addr) => {
            tracing::info!("Listening on {addr}");
            true
        }
        Err(e) => {
            let message = format!("{e:#}");
            tracing::error!("Unable to start server: {message}");
            tray::notify(Level::Error, "Unable to start server", &message);
            false
        }
    }
}

/// Re-read config.json and restart the server. If the file is invalid the running
/// server is kept as-is.
fn restart(state: &AppState, server: &ServerController) {
    if let Err(e) = state.reload() {
        tracing::error!("{e:#}");
        tray::notify(Level::Error, "Restart failed", &format!("{e:#}"));
        return;
    }
    logging::set_file_enabled(state.config().log.enabled);
    server.stop();
    if start_server(server) {
        let config = state.config();
        let message = format!(
            "Listening on {}:{}, {} printer alias(es)",
            config.server.bind,
            config.server.port,
            config.printer.mappings.len()
        );
        tray::notify(Level::Info, "Settings applied", &message);
    }
}
