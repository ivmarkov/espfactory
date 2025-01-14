use std::sync::Mutex;

use alloc::sync::Arc;

use log::{Level, Log, Metadata, Record};

use crate::model::Model;

extern crate alloc;

/// The global logger used by the factory
pub static LOGGER: Logger = Logger::new();

/// The logger used by `espfactory`
///
/// What it does:
/// - Writes all logs to a file
/// - Keeps the last N log lines in a memory buffer (for rendering in the UI)
/// - Signals when a log message has been written
pub struct Logger(Mutex<Option<Arc<Model>>>);

impl Default for Logger {
    fn default() -> Self {
        Self::new()
    }
}

impl Logger {
    /// Create a new `Logger`
    pub const fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// Swap the current model in the logger (if any) with a new one or `None`
    pub fn swap_model(&self, model: Option<Arc<Model>>) -> Option<Arc<Model>> {
        let mut guard = self.0.lock().unwrap();

        core::mem::replace(&mut guard, model)
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() >= Level::Info
    }

    fn log(&self, record: &Record) {
        if let Some(model) = self.0.lock().unwrap().clone() {
            model.access_mut(|inner| {
                inner.logs.file.log(record);

                ((), inner.logs.buffered.log(record))
            });
        }
    }

    fn flush(&self) {
        if let Some(model) = self.0.lock().unwrap().clone() {
            model.access_mut(|inner| {
                inner.logs.file.flush();
                ((), false)
            });
        }
    }
}
