//! `config.json` handling.
//!
//! The file format is identical to the Java bridge this is ported from, so an existing
//! `config.json` can be reused as-is. Keys this port does not use (`server.address`,
//! `server.tls`, `serial`, ...) are ignored on load and preserved on save. `log` is an
//! addition of this port.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub gui: Gui,
    pub server: Server,
    pub downloader: Downloader,
    pub printer: Printer,
    pub log: Log,
}

/// `log/client.log`; mainly useful for troubleshooting.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Log {
    pub enabled: bool,
}

impl Default for Log {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Gui {
    pub notification: Notification,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Notification {
    pub enabled: bool,
}

impl Default for Notification {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Server {
    pub address: String,
    pub bind: String,
    pub port: u16,
    pub authentication: Authentication,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            address: "127.0.0.1".into(),
            bind: "127.0.0.1".into(),
            port: 12212,
            authentication: Authentication::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Authentication {
    pub enabled: bool,
    pub token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Downloader {
    #[serde(rename = "ignoreTLSCertificateError")]
    pub ignore_tls_certificate_error: bool,
    /// Connect and read timeout in seconds.
    pub timeout: f64,
    pub path: String,
}

impl Default for Downloader {
    fn default() -> Self {
        Self {
            ignore_tls_certificate_error: false,
            timeout: 30.0,
            path: "downloads".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Printer {
    pub enabled: bool,
    pub auto_add_unknown_type: bool,
    pub fallback_to_default: bool,
    pub mappings: Vec<PrinterMapping>,
}

impl Default for Printer {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_add_unknown_type: false,
            fallback_to_default: false,
            mappings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PrinterMapping {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub auto_rotate: bool,
    pub reset_imageable_area: bool,
    #[serde(rename = "forceDPI")]
    pub force_dpi: u32,
}

impl Default for PrinterMapping {
    fn default() -> Self {
        Self {
            kind: String::new(),
            name: String::new(),
            auto_rotate: false,
            reset_imageable_area: true,
            force_dpi: 0,
        }
    }
}

pub fn config_path() -> PathBuf {
    PathBuf::from("config.json")
}

/// Load the config file. A missing file is created with defaults (like the Java app);
/// an unreadable or invalid file is reported as an error and left untouched.
pub fn load(path: &Path) -> Result<Config> {
    if !path.exists() {
        tracing::warn!(
            "Config file {} not found, creating new file",
            path.display()
        );
        let config = Config::default();
        let json = serde_json::to_string_pretty(&config)?;
        std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
        return Ok(config);
    }

    tracing::info!("Loading config from file: {}", path.display());
    let text = read_text(path)?;
    let config: Config =
        serde_json::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
    Ok(config)
}

/// Read the file as UTF-8, ignoring a byte order mark (Notepad and PowerShell write one;
/// Jackson accepted it too).
fn read_text(path: &Path) -> Result<String> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text
        .strip_prefix('\u{feff}')
        .map(str::to_owned)
        .unwrap_or(text))
}

/// Append a placeholder mapping for `kind` to the config file (`autoAddUnknownType`).
/// The file is edited as raw JSON so keys this port does not know about survive.
pub fn add_print_type(path: &Path, kind: &str) -> Result<()> {
    let mut root: Value = serde_json::from_str(&read_text(path)?)?;

    let mapping = serde_json::to_value(PrinterMapping {
        kind: kind.to_string(),
        ..PrinterMapping::default()
    })?;
    object_at(&mut root, &["printer"])?
        .entry("mappings")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("printer.mappings is not an array")?
        .push(mapping);

    write_json(path, &root)
}

/// Write the settings edited in the settings window. Only the keys the window manages
/// are replaced; everything else in the file (downloader, tls, serial, ...) is kept.
pub fn save_settings(path: &Path, config: &Config) -> Result<()> {
    let mut root: Value = if path.exists() {
        serde_json::from_str(&read_text(path)?)
            .with_context(|| format!("invalid {}", path.display()))?
    } else {
        Value::Object(Default::default())
    };

    object_at(&mut root, &["gui", "notification"])?
        .insert("enabled".into(), config.gui.notification.enabled.into());

    let server = object_at(&mut root, &["server"])?;
    server.insert("bind".into(), config.server.bind.clone().into());
    server.insert("port".into(), config.server.port.into());
    let auth = object_at(&mut root, &["server", "authentication"])?;
    auth.insert(
        "enabled".into(),
        config.server.authentication.enabled.into(),
    );
    auth.insert(
        "token".into(),
        serde_json::to_value(&config.server.authentication.token)?,
    );

    let printer = object_at(&mut root, &["printer"])?;
    printer.insert(
        "autoAddUnknownType".into(),
        config.printer.auto_add_unknown_type.into(),
    );
    printer.insert(
        "fallbackToDefault".into(),
        config.printer.fallback_to_default.into(),
    );
    printer.insert(
        "mappings".into(),
        serde_json::to_value(&config.printer.mappings)?,
    );

    object_at(&mut root, &["log"])?.insert("enabled".into(), config.log.enabled.into());

    write_json(path, &root)
}

/// Problems that make a configuration unusable, as user-facing messages.
pub fn validate(config: &Config) -> Vec<String> {
    let mut problems = Vec::new();
    for (i, mapping) in config.printer.mappings.iter().enumerate() {
        if mapping.kind.trim().is_empty() {
            problems.push(format!("Alias #{} has no name.", i + 1));
        } else if config.printer.mappings[..i]
            .iter()
            .any(|m| m.kind == mapping.kind)
        {
            problems.push(format!("The alias \"{}\" is listed twice.", mapping.kind));
        }
    }
    if config.server.bind.trim().is_empty() {
        problems.push("The bind address is empty.".into());
    }
    if config.server.port == 0 {
        problems.push("The port must be between 1 and 65535.".into());
    }
    if config.server.authentication.enabled
        && config
            .server
            .authentication
            .token
            .as_deref()
            .is_none_or(|t| t.is_empty())
    {
        problems.push("A token is required when \"Require token\" is enabled.".into());
    }
    problems
}

/// The JSON object at `path` below `root`, created (as `{}`) where missing.
fn object_at<'a>(
    root: &'a mut Value,
    path: &[&str],
) -> Result<&'a mut serde_json::Map<String, Value>> {
    let mut current = root;
    for key in path {
        current = current
            .as_object_mut()
            .with_context(|| format!("config: parent of \"{key}\" is not an object"))?
            .entry(*key)
            .or_insert_with(|| Value::Object(Default::default()));
    }
    current
        .as_object_mut()
        .with_context(|| format!("config: \"{}\" is not an object", path.join(".")))
}

fn write_json(path: &Path, root: &Value) -> Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(root)?)
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_java_config() {
        let json = r#"{
          "gui" : { "notification" : { "enabled" : false } },
          "server" : {
            "address" : "192.168.1.20", "bind" : "192.168.1.20", "port" : 12212,
            "authentication" : { "enabled" : false, "token" : null },
            "tls" : { "enabled" : false, "selfSigned" : true, "cert" : "c", "key" : "k", "caBundle" : null }
          },
          "downloader" : { "ignoreTLSCertificateError" : true, "timeout" : 30.0, "path" : "downloads" },
          "printer" : {
            "enabled" : true, "autoAddUnknownType" : false, "fallbackToDefault" : false,
            "mappings" : [ { "type" : "LABEL", "name" : "Microsoft Print to PDF",
                             "autoRotate" : true, "resetImageableArea" : false, "forceDPI" : 300 } ]
          },
          "serial" : { "enabled" : true, "mappings" : [ ] }
        }"#;
        let c: Config = serde_json::from_str(json).unwrap();
        assert!(!c.gui.notification.enabled);
        assert_eq!(c.server.bind, "192.168.1.20");
        assert!(c.downloader.ignore_tls_certificate_error);
        let m = &c.printer.mappings[0];
        assert_eq!(
            (m.kind.as_str(), m.name.as_str()),
            ("LABEL", "Microsoft Print to PDF")
        );
        assert!(m.auto_rotate && !m.reset_imageable_area);
        assert_eq!(m.force_dpi, 300);
    }

    #[test]
    fn defaults_match_java() {
        let c: Config = serde_json::from_str("{}").unwrap();
        assert!(c.gui.notification.enabled);
        assert_eq!(
            (c.server.bind.as_str(), c.server.port),
            ("127.0.0.1", 12212)
        );
        assert_eq!(c.downloader.timeout, 30.0);
        let m: PrinterMapping = serde_json::from_str(r#"{"type":"X","name":"Y"}"#).unwrap();
        assert!(m.reset_imageable_area && !m.auto_rotate && m.force_dpi == 0);
    }
}
