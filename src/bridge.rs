//! Print job pipeline shared by the WebSocket server: printer lookup, document
//! preparation, dispatch to the printing backends, result and notifications.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::config::{self, Config, PrinterMapping};
use crate::notify::{self, Level};
use crate::printing::{self, JobOptions};
use crate::protocol::{PrintDocument, PrintResult, decode_base64_lenient};

pub struct AppState {
    config: RwLock<Arc<Config>>,
    config_path: PathBuf,
}

impl AppState {
    pub fn new(config: Config, config_path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            config: RwLock::new(Arc::new(config)),
            config_path,
        })
    }

    pub fn config(&self) -> Arc<Config> {
        self.config.read().unwrap().clone()
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Re-read config.json; on error the current config stays active.
    pub fn reload(&self) -> Result<()> {
        let config = config::load(&self.config_path)?;
        *self.config.write().unwrap() = Arc::new(config);
        Ok(())
    }

    fn add_print_type(&self, kind: &str) {
        let mut guard = self.config.write().unwrap();
        if guard.printer.mappings.iter().any(|m| m.kind == kind) {
            return;
        }
        let mut config = (**guard).clone();
        config.printer.mappings.push(PrinterMapping {
            kind: kind.to_string(),
            ..Default::default()
        });
        *guard = Arc::new(config);
        if let Err(e) = config::add_print_type(&self.config_path, kind) {
            tracing::error!("Failed to save config file: {e:#}");
        }
    }
}

struct Target {
    name: String,
    mapping: PrinterMapping,
}

/// Handle one text message from a `/printer` socket. Returns the result to broadcast,
/// or `None` if the message was not a valid print request (logged only, like Java).
pub async fn handle_message(state: Arc<AppState>, text: String) -> Option<PrintResult> {
    match serde_json::from_str::<PrintDocument>(&text) {
        Ok(doc) => Some(print_document(state, doc).await),
        Err(e) => {
            tracing::error!("Invalid print request: {e}");
            None
        }
    }
}

async fn print_document(state: Arc<AppState>, doc: PrintDocument) -> PrintResult {
    let kind = doc.kind.clone().unwrap_or_else(|| "null".into());
    tracing::info!(
        "Printing Document {kind}, {}",
        doc.url.as_deref().unwrap_or("null")
    );

    let download_path = output_path(&state.config(), &doc, &uuid::Uuid::new_v4().to_string());
    let mut printer_name = None;

    let result: Result<()> = async {
        let target = {
            let state = state.clone();
            let kind = doc.kind.clone();
            tokio::task::spawn_blocking(move || search_printer(&state, kind.as_deref())).await??
        };
        printer_name = Some(target.name.clone());

        notify_print(
            &state,
            Level::Info,
            &format!("Printing {kind}"),
            doc.url.as_deref().unwrap_or(""),
        );

        let job_name;
        let data = if doc.is_raw() {
            job_name = format!("{} {kind}", crate::APP_NAME);
            decode_base64_lenient(doc.raw_content.as_deref().unwrap_or(""))
                .context("Invalid raw_content")?
        } else if doc.is_image() || doc.is_pdf() {
            let path = download_path
                .as_ref()
                .map_err(|e| anyhow!("{e:#}"))?
                .clone();
            job_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            prepare_document(&state.config(), &doc, &path).await?
        } else {
            bail!(
                "Unknown file type: {}",
                doc.url.as_deref().unwrap_or("null")
            );
        };

        let started = std::time::Instant::now();
        let is_raw = doc.is_raw();
        let is_image = doc.is_image();
        let copies = doc.copies();
        let annotations = doc.extras.clone();
        tokio::task::spawn_blocking(move || {
            let opts = JobOptions {
                job_name: &job_name,
                copies,
                mapping: &target.mapping,
                annotations: &annotations,
                output_file: None,
            };
            if is_raw {
                printing::print_raw(&target.name, &job_name, &data)
            } else if is_image {
                printing::print_image(&target.name, &data, &opts)
            } else {
                printing::print_pdf(&target.name, &data, &opts)
            }
        })
        .await??;
        tracing::info!("Print job finished in {} ms", started.elapsed().as_millis());
        Ok(())
    }
    .await;

    match result {
        Ok(()) => PrintResult {
            success: true,
            message: Some("Success".into()),
            id: doc.id,
            printer_name,
        },
        Err(e) => {
            let message = format!("{e:#}");
            tracing::error!("Print Error: {message}");
            if !doc.is_raw()
                && let Ok(path) = &download_path
            {
                let _ = tokio::fs::remove_file(path).await;
            }
            notify_print(
                &state,
                Level::Error,
                &format!("Print Error {kind}"),
                &message,
            );
            PrintResult {
                success: false,
                message: Some(message),
                id: doc.id,
                printer_name,
            }
        }
    }
}

fn notify_print(state: &AppState, level: Level, title: &str, message: &str) {
    if state.config().gui.notification.enabled {
        notify::show(level, title, message);
    }
}

/// Find the printer for a document type: mapped printer (name compared case-insensitively),
/// then optional auto-add of the unknown type, then optional default printer.
fn search_printer(state: &AppState, kind: Option<&str>) -> Result<Target> {
    let config = state.config();
    let mapping = config
        .printer
        .mappings
        .iter()
        .find(|m| Some(m.kind.as_str()) == kind);

    if let Some(mapping) = mapping {
        let wanted = mapping.name.to_lowercase();
        if let Some(name) = printing::list_printers()?
            .into_iter()
            .find(|p| p.to_lowercase() == wanted)
        {
            tracing::info!(
                "Sending print job type: {} to printer: {name}",
                mapping.kind
            );
            return Ok(Target {
                name,
                mapping: mapping.clone(),
            });
        }
    }

    if config.printer.auto_add_unknown_type
        && mapping.is_none()
        && let Some(kind) = kind
    {
        state.add_print_type(kind);
    }

    if config.printer.fallback_to_default {
        tracing::info!(
            "No mapped print job type: {}, falling back to default printer",
            kind.unwrap_or("null")
        );
        let name =
            printing::default_printer().ok_or_else(|| anyhow!("No default printer found"))?;
        return Ok(Target {
            name,
            mapping: PrinterMapping::default(),
        });
    }

    bail!("No matched printer: {}", kind.unwrap_or("null"))
}

/// `downloads/<uuid>-<file name>`; the name comes from the URL path, or from `url`
/// itself when the file is sent inline as `file_content`.
fn output_path(config: &Config, doc: &PrintDocument, uuid: &str) -> Result<PathBuf> {
    let name = if doc.file_content.is_some() {
        doc.url.clone().unwrap_or_else(|| "null".into())
    } else {
        let url = doc
            .url
            .as_deref()
            .ok_or_else(|| anyhow!("Both URL and File Content are null"))?;
        let url = reqwest::Url::parse(url).with_context(|| format!("Invalid URL: {url}"))?;
        url.path().to_string()
    };
    let name = name.rsplit(['/', '\\']).next().unwrap_or("");
    let name: String = name
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    Ok(Path::new(&config.downloader.path).join(format!("{uuid}-{name}")))
}

/// Write the document to `path` (kept in the downloads folder, like the Java bridge)
/// and return its bytes.
async fn prepare_document(config: &Config, doc: &PrintDocument, path: &Path) -> Result<Vec<u8>> {
    tokio::fs::create_dir_all(&config.downloader.path)
        .await
        .with_context(|| format!("Cannot create {}", config.downloader.path))?;

    let bytes = if let Some(content) = &doc.file_content {
        decode_base64_lenient(content).context("Invalid file_content")?
    } else {
        let url = doc
            .url
            .as_deref()
            .ok_or_else(|| anyhow!("Both URL and File Content are null"))?;
        download(config, url).await?
    };
    tokio::fs::write(path, &bytes)
        .await
        .with_context(|| format!("Cannot write {}", path.display()))?;
    Ok(bytes)
}

async fn download(config: &Config, url: &str) -> Result<Vec<u8>> {
    tracing::info!("Downloading file from: {url}");
    let started = std::time::Instant::now();
    let timeout = Duration::from_secs_f64(config.downloader.timeout.max(1.0));
    let client = reqwest::Client::builder()
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .tls_danger_accept_invalid_certs(config.downloader.ignore_tls_certificate_error)
        .build()?;

    let response = client.get(url).send().await?;
    if response.status().as_u16() != 200 {
        bail!("HTTP Status Code: {}", response.status().as_u16());
    }
    let bytes = response.bytes().await?.to_vec();
    tracing::info!(
        "Downloaded {} bytes in {} ms",
        bytes.len(),
        started.elapsed().as_millis()
    );
    Ok(bytes)
}
