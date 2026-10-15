//! Web apps print through a local WebSocket on `/printer`.

pub mod bridge;
pub mod config;
pub mod install;
pub mod logging;
pub mod netinfo;
pub mod notify;
pub mod printing;
pub mod protocol;
pub mod server;
pub mod settings;
pub mod tray;

pub const APP_NAME: &str = "WebApp Printer Bridge";
pub const APP_ID: &str = "joffs.webapp-printer-bridge";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
