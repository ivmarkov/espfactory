use std::io::Write;
use std::sync::Mutex;

use alloc::collections::VecDeque;
use alloc::sync::Arc;

use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, RawMutex};
use embassy_sync::signal::Signal;

use log::{Level, LevelFilter, Log, Metadata, Record};

extern crate alloc;

pub type LogFile = tempfile::NamedTempFile;

/// The global logger used by the factory
pub static LOGGER: Logger<LogFile, Arc<Signal<CriticalSectionRawMutex, ()>>> =
    Logger::new(LevelFilter::Debug, LevelFilter::Info, 10);

/// A trait for signaling that a log message has been written
pub trait LogSignal {
    /// Signal that a log message has been written
    fn signal(&self);
}

impl<T> LogSignal for &T
where
    T: LogSignal,
{
    fn signal(&self) {
        (**self).signal();
    }
}

impl<T> LogSignal for Arc<Signal<T, ()>>
where
    T: RawMutex + Send,
{
    fn signal(&self) {
        self.as_ref().signal(());
    }
}

/// The logger used by `espfactory`
///
/// What it does:
/// - Writes all logs to a file
/// - Keeps the last N log lines in a memory buffer (for rendering in the UI)
/// - Signals when a log message has been written
pub struct Logger<T, S> {
    inner: Mutex<LoggerState<T>>,
    signal: Mutex<Option<S>>,
}

impl<T, S> Logger<T, S>
where
    T: Write,
    S: LogSignal,
{
    /// Create a new `Logger`
    ///
    /// # Arguments
    /// - `level` - the log level to use overall (for writing to the file as well as for keeping in memory)
    /// - `last_n_level` - the log level to use for keeping the last N log lines in memory (should be higher or equal to the overall log level)
    /// - `last_n_len` - the number of last N log lines to keep in memory
    pub const fn new(level: LevelFilter, last_n_level: LevelFilter, last_n_len: usize) -> Self {
        Self {
            inner: Mutex::new(LoggerState {
                level,
                last_n: VecDeque::new(),
                last_n_len,
                last_n_level,
                out: None,
            }),
            signal: Mutex::new(None),
        }
    }

    /// Swaps the existing signal used by the logger (if any) with the provided one
    /// The signal will be called any time a log message is written
    ///
    /// Returns the previous signal, if any
    ///
    /// # Arguments
    /// - `signal` - the new signal to use or `None` to remove the existing signal
    pub fn swap_signal(&self, signal: Option<S>) -> Option<S> {
        let mut guard = self.signal.lock().unwrap();

        core::mem::replace(&mut guard, signal)
    }

    /// Locks the logger and returns a guard to the logger state
    ///
    /// The logger is locked only if it is not already locked by the current thread
    ///
    /// Returns `None` if the logger is already locked by the current thread
    /// or `Some` with a guard to the logger state otherwise
    pub fn lock<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut LoggerState<T>) -> R,
    {
        let mut guard = self.inner.lock().unwrap();

        f(&mut guard)
    }
}

impl<T, S> Log for Logger<T, S>
where
    T: Write + Send,
    S: LogSignal + Send,
{
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if self.lock(|logger| logger.log(record)) {
            // TODO: Figure out why signalling leads to a deadlock
            let signal = self.signal.lock().unwrap();
            if let Some(signal) = signal.as_ref() {
                signal.signal();
            }
        }
    }

    fn flush(&self) {
        // TODO
    }
}

/// The state of a `Logger` instance
pub struct LoggerState<T> {
    level: LevelFilter,
    last_n: VecDeque<LogLine>,
    last_n_len: usize,
    last_n_level: LevelFilter,
    out: Option<T>,
}

impl<T> LoggerState<T>
where
    T: Write,
{
    /// Set the log level to use overall
    pub fn set_level(&mut self, level: LevelFilter) {
        self.level = level;
    }

    /// Swaps the existing output used by the logger (if any) with the provided one
    ///
    /// Returns the previous output, if any
    ///
    /// # Arguments
    /// - `out` - the new output to use or `None` to remove the existing output
    pub fn swap_out(&mut self, out: Option<T>) -> Option<T> {
        core::mem::replace(&mut self.out, out)
    }

    /// Get an iterator over the last `n` log lines kept in memory
    ///
    /// # Arguments
    /// - `n` - the number of last log lines to get
    pub fn last_n(&self, n: usize) -> impl Iterator<Item = &LogLine> {
        self.last_n.iter().skip(if self.last_n.len() > n {
            self.last_n.len() - n
        } else {
            0
        })
    }

    fn log(&mut self, record: &Record) -> bool {
        if self.level >= record.level() {
            if let Some(out) = self.out.as_mut() {
                let message = format!(
                    "[{} {} {}] {}",
                    record.level(),
                    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
                    record.target(),
                    record.args()
                );

                let _ = out.write_all(message.as_bytes());
                let _ = out.write_all(b"\n");
            }

            if self.last_n_level >= record.level() {
                for line in format!("{}", record.args()).lines() {
                    self.push(LogLine {
                        level: record.level(),
                        message: line.to_string(),
                    });
                }

                return true;
            }
        }

        false
    }

    fn push(&mut self, msg: LogLine) {
        if self.last_n.len() >= self.last_n_len {
            self.last_n.pop_front();
        }

        self.last_n.push_back(msg);
    }
}

/// A log line kept in memory
#[derive(Debug, Clone)]
pub struct LogLine {
    /// The log level of the line
    pub level: Level,
    /// The message to be displayed on that line
    pub message: String,
}

pub mod file {
    use std::io::{Read, Write};

    use tempfile::NamedTempFile;

    use zip::{write::FileOptions, ZipWriter};

    use super::LogFile;

    pub fn start() -> anyhow::Result<LogFile> {
        let log = NamedTempFile::new()?;

        Ok(log)
    }

    pub fn finish<'i, I, S>(mut log: LogFile, summary: I) -> anyhow::Result<impl Read>
    where
        I: IntoIterator<Item = (&'i S, &'i S)>,
        S: AsRef<str> + 'i,
    {
        log.flush()?;

        let mut log_zip_file = NamedTempFile::new()?;

        let mut log_zip = ZipWriter::new(&mut log_zip_file);
        log_zip.start_file("log.txt", FileOptions::<()>::default())?;

        log.reopen()?;
        std::io::copy(&mut log, &mut log_zip)?;

        log_zip.start_file("log.csv", FileOptions::<()>::default())?;

        let mut csv = csv::WriterBuilder::new()
            .has_headers(true)
            .from_writer(&mut log);

        csv.serialize(("Name", "Value"))?;

        for (name, value) in summary {
            csv.serialize((name.as_ref(), value.as_ref()))?;
        }

        csv.flush()?;

        drop(log_zip);

        log_zip_file.reopen()?;

        Ok(log_zip_file)
    }
}
