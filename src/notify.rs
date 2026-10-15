//! Desktop notifications. The tray installs the sink; without one, notifications are only logged.

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warning,
    Error,
}

type Sink = Box<dyn Fn(Level, &str, &str) + Send + Sync>;

static SINK: OnceLock<Sink> = OnceLock::new();

pub fn set_sink(sink: impl Fn(Level, &str, &str) + Send + Sync + 'static) {
    let _ = SINK.set(Box::new(sink));
}

pub fn show(level: Level, title: &str, message: &str) {
    if let Some(sink) = SINK.get() {
        sink(level, title, message);
    }
}
