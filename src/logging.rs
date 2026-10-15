//! Logging to `log/client.log`, rolled over at 10 MB keeping 5 old files
//! (same policy as the Java bridge's log4j2 setup). The file can be switched off
//! (`log.enabled` in config.json); lines logged before the config is read are kept in
//! memory until [`set_file_enabled`] decides where they go.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

const MAX_SIZE: u64 = 10 * 1024 * 1024;
const MAX_BACKUPS: u32 = 5;

pub struct RollingFile {
    path: PathBuf,
    file: File,
    size: u64,
}

impl RollingFile {
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata()?.len();
        Ok(Self { path, file, size })
    }

    fn backup(&self, index: u32) -> PathBuf {
        let stem = self.path.file_stem().unwrap_or_default().to_string_lossy();
        let ext = self
            .path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        self.path.with_file_name(format!("{stem}-{index}{ext}"))
    }

    fn roll(&mut self) -> io::Result<()> {
        let _ = std::fs::remove_file(self.backup(MAX_BACKUPS));
        for i in (1..MAX_BACKUPS).rev() {
            let _ = std::fs::rename(self.backup(i), self.backup(i + 1));
        }
        self.file.flush()?;
        std::fs::rename(&self.path, self.backup(1))?;
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.size = 0;
        Ok(())
    }
}

impl Write for RollingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.size + buf.len() as u64 > MAX_SIZE && self.size > 0 {
            let _ = self.roll();
        }
        let n = self.file.write(buf)?;
        self.size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

struct LocalTime;

impl FormatTime for LocalTime {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(
            w,
            "{}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f")
        )
    }
}

/// Where log lines go.
enum Sink {
    /// Before the config is read: buffered (bounded) until [`set_file_enabled`].
    Pending(Vec<u8>),
    File(RollingFile),
    Off,
}

const PENDING_LIMIT: usize = 256 * 1024;

struct LogState {
    path: PathBuf,
    sink: Mutex<Sink>,
}

static STATE: OnceLock<LogState> = OnceLock::new();

/// Writer handed to tracing; each write goes to the current [`Sink`].
struct LogWriter;

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(state) = STATE.get() {
            match &mut *state.sink.lock().unwrap() {
                Sink::Pending(pending) if pending.len() + buf.len() <= PENDING_LIMIT => {
                    pending.extend_from_slice(buf)
                }
                Sink::File(file) => file.write_all(buf)?,
                Sink::Pending(_) | Sink::Off => {}
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(state) = STATE.get()
            && let Sink::File(file) = &mut *state.sink.lock().unwrap()
        {
            file.flush()?;
        }
        Ok(())
    }
}

/// Set up logging to `log_dir/client.log`. Nothing is written to disk until
/// [`set_file_enabled`] is called (lines are buffered until then).
pub fn init(log_dir: &Path) {
    let _ = STATE.set(LogState {
        path: log_dir.join("client.log"),
        sink: Mutex::new(Sink::Pending(Vec::new())),
    });
    tracing_subscriber::fmt()
        .with_timer(LocalTime)
        .with_target(false)
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(|| LogWriter)
        .init();

    std::panic::set_hook(Box::new(|info| {
        tracing::error!("panic: {info}");
    }));
}

/// Switch the log file on or off. The first call also writes (or drops) the lines buffered
/// since [`init`]. If the file cannot be opened, logging stays off.
pub fn set_file_enabled(enabled: bool) {
    let Some(state) = STATE.get() else { return };
    let (is_file, was_off) = match *state.sink.lock().unwrap() {
        Sink::File(_) => (true, false),
        Sink::Off => (false, true),
        Sink::Pending(_) => (false, false),
    };
    if is_file && !enabled {
        // Leave a hint in the file why it ends here.
        tracing::info!("Log file disabled in settings");
    }

    let mut sink = state.sink.lock().unwrap();
    let pending = match &mut *sink {
        Sink::Pending(buf) => Some(std::mem::take(buf)),
        Sink::File(_) if enabled => return,
        Sink::Off if !enabled => return,
        _ => None,
    };
    *sink = if enabled {
        match RollingFile::open(&state.path) {
            Ok(mut file) => {
                if let Some(pending) = pending {
                    let _ = file.write_all(&pending);
                }
                Sink::File(file)
            }
            Err(_) => Sink::Off,
        }
    } else {
        Sink::Off
    };
    let opened = matches!(*sink, Sink::File(_));
    drop(sink);
    if opened && was_off {
        tracing::info!("Log file enabled in settings");
    }
}
