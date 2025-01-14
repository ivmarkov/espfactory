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

impl Logger {
    /// Create a new `Logger`
    ///
    /// # Arguments
    /// - `level` - the log level to use overall (for writing to the file as well as for keeping in memory)
    /// - `last_n_level` - the log level to use for keeping the last N log lines in memory (should be higher or equal to the overall log level)
    /// - `last_n_len` - the number of last N log lines to keep in memory
    pub const fn new() -> Self {
        Self(Mutex::new(None))
    }

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
