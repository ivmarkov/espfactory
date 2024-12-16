use std::fs::File;
use std::io::{self, Write};
use std::sync::{Mutex, MutexGuard};

use alloc::collections::VecDeque;
use alloc::sync::Arc;

use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, RawMutex};
use embassy_sync::signal::Signal;

use log::{Level, Log, Metadata, Record};

extern crate alloc;

/// The global logger used by the factory
pub static LOGGER: Logger<File, Arc<Signal<CriticalSectionRawMutex, ()>>> =
    Logger::new(Level::Debug, Level::Info, 10);

/// A trait for signaling that a log message has been written
pub trait LogSignal {
    /// Signal that a log message has been written
    fn signal(&mut self);
}

impl<T> LogSignal for &mut T
where
    T: LogSignal,
{
    fn signal(&mut self) {
        (**self).signal();
    }
}

impl<T> LogSignal for Arc<Signal<T, ()>>
where
    T: RawMutex + Send,
{
    fn signal(&mut self) {
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
    inner: Mutex<LoggerState<T, S>>,
    level: Level,
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
    pub const fn new(level: Level, last_n_level: Level, last_n_len: usize) -> Self {
        Self {
            inner: Mutex::new(LoggerState {
                last_n: VecDeque::new(),
                last_n_len,
                last_n_level,
                out: None,
                signal: None,
            }),
            level,
        }
    }

    /// Locks the logger and
    pub fn lock(&self) -> MutexGuard<LoggerState<T, S>> {
        self.inner.lock().unwrap()
    }
}

impl<T, S> Log for Logger<T, S>
where
    T: Write + Send,
    S: LogSignal + Send,
{
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        self.lock().log(record).unwrap();
    }

    fn flush(&self) {
        // TODO
    }
}

/// The state of a `Logger` instance
pub struct LoggerState<T, S> {
    last_n: VecDeque<LogLine>,
    last_n_len: usize,
    last_n_level: Level,
    out: Option<T>,
    signal: Option<S>,
}

impl<T, S> LoggerState<T, S>
where
    T: Write,
    S: LogSignal,
{
    /// Swaps the existing signal used by the logger (if any) with the provided one
    /// The signal will be called any time a log message is written
    ///
    /// Returns the previous signal, if any
    ///
    /// # Arguments
    /// - `signal` - the new signal to use or `None` to remove the existing signal
    pub fn swap_signal(&mut self, signal: Option<S>) -> Option<S> {
        core::mem::replace(&mut self.signal, signal)
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

    fn log(&mut self, record: &Record) -> io::Result<()> {
        let message = format!("[{}] {}", record.level(), record.args());

        if let Some(out) = self.out.as_mut() {
            out.write_all(message.as_bytes())?;
            out.write_all(b"\n")?;
        }

        if self.last_n_level >= record.level() {
            for line in message.lines() {
                self.push(LogLine {
                    level: record.level(),
                    message: line.to_string(),
                });
            }
        }

        if let Some(signal) = self.signal.as_mut() {
            signal.signal();
        }

        Ok(())
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