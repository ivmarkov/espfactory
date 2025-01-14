use core::cell::RefCell;
use core::num::Wrapping;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write as _};

use alloc::sync::Arc;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::signal::Signal;

use log::{LevelFilter, Record};

use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};
use tempfile::tempfile;
use zip::write::FileOptions;
use zip::ZipWriter;

use crate::bundle::Bundle;

extern crate alloc;

/// The model of the factory application
pub struct Model {
    /// The state of the model (i.e. awating readouts, displaying an error, preparing the bundle, flashing it etc. etc.)
    state: Mutex<CriticalSectionRawMutex, RefCell<ModelInner>>, // TODO: Change to std::sync::Mutex?
    /// A signal to notify that the model has changed
    /// Used to trigger redraws of the UI
    changed: Arc<Signal<CriticalSectionRawMutex, ()>>,
}

impl Model {
    /// Create a new model in the initial state (readouts)
    pub const fn new(
        level: LevelFilter,
        width: u16,
        height: u16,
        changed: Arc<Signal<CriticalSectionRawMutex, ()>>,
    ) -> Self {
        Self {
            state: Mutex::new(RefCell::new(ModelInner::new(level, width, height))),
            changed,
        }
    }

    /// Get the current state of the model in the given closure
    pub fn access<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&ModelInner) -> R,
    {
        self.state
            .lock(|inner: &RefCell<ModelInner>| f(&inner.borrow()))
    }

    /// Modify the state of the model by applying the given closure to it
    pub fn modify<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut ModelInner) -> R,
    {
        self.access_mut(|inner| (f(inner), true))
    }

    /// Accwess the state of the model by applying the given closure to it
    /// If the closure returns `true`, the model is considered to have changed and the `changed` signal is triggered
    pub fn access_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut ModelInner) -> (R, bool),
    {
        self.state.lock(|inner| {
            let mut inner = inner.borrow_mut();

            let (result, modified) = f(&mut inner);

            if modified {
                self.changed.signal(());
            }

            result
        })
    }

    /// Wait for the model to change
    /// The UI is expected to call this method to wait for the model to change before redrawing
    pub async fn wait_changed(&self) {
        self.changed.wait().await;
    }
}

#[derive(Debug)]
pub struct ModelInner {
    pub state: State,
    pub logs: Logs,
}

impl ModelInner {
    pub const fn new(level: LevelFilter, width: u16, height: u16) -> Self {
        Self {
            state: State::new(),
            logs: Logs::new(level, BufferedLogsLayout::Bottom, width, height),
        }
    }
}

/// The state of the model
#[derive(Debug)]
pub enum State {
    /// The model is presenting the eFuse readouts and awaiting user readouts
    Readout(Readout),
    /// The model has prepared the bundle and is either waiting for user confirmation or provisioning it already
    Provision(Provision),
    /// The model is processing a task
    Processing(Processing),
    /// The model needs to present the outcome of a task (success or failure)
    Status(Status),
}

impl State {
    /// Create a new state in the `Processing` state
    pub const fn new() -> Self {
        Self::Processing(Processing::empty())
    }

    pub fn success(&mut self, title: impl Into<String>, message: impl Into<String>) {
        *self = Self::Status(Status::success(title, message));
    }

    pub fn error(&mut self, title: impl Into<String>, message: impl Into<String>) {
        *self = Self::Status(Status::error(title, message));
    }

    /// Get a reference to the readout from the state
    /// Panics if the state is not `Readout`
    pub fn readout(&self) -> &Readout {
        if let Self::Readout(readouts) = self {
            readouts
        } else {
            panic!("Unexpected state: {self:?}")
        }
    }

    /// Get a mutable reference to the readout from the state
    /// Panics if the state is not `Readout`
    pub fn readout_mut(&mut self) -> &mut Readout {
        if let Self::Readout(readouts) = self {
            readouts
        } else {
            panic!("Unexpected state: {self:?}")
        }
    }

    /// Get a reference to the provision state
    /// Panics if the state is not `Provision`
    pub fn provision(&self) -> &Provision {
        if let Self::Provision(provision) = self {
            provision
        } else {
            panic!("Unexpected state: {self:?}")
        }
    }

    /// Get a mutable reference to the provision state
    /// Panics if the state is not `Provision`
    pub fn provision_mut(&mut self) -> &mut Provision {
        if let Self::Provision(provision) = self {
            provision
        } else {
            panic!("Unexpected state: {self:?}")
        }
    }

    /// Get a mutable reference to the processing state
    /// Panics if the state is not `Processing`
    pub fn processing_mut(&mut self) -> &mut Processing {
        if let Self::Processing(processing) = self {
            processing
        } else {
            panic!("Unexpected state: {self:?}")
        }
    }

    /// Get a mutable reference to the status state
    /// Panics if the state is not `Status`
    pub fn status_mut(&mut self) -> &mut Status {
        if let Self::Status(status) = self {
            status
        } else {
            panic!("Unexpected state: {self:?}")
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

/// The readouts state of the model
#[derive(Debug, Clone)]
pub struct Readout {
    /// The eFuse readouts to display
    /// Each readout is a tuple of the eFuse key and its stringified value
    pub efuse_readouts: Vec<(String, String)>,
    /// The readouts to display and input
    /// Each readout is a tuple of the readout name and the readout value
    pub readouts: Vec<(String, String)>,
    /// The index of the active readout
    /// Used to indicate which readout is currently being input
    /// When all readouts are input, this is equal to the length of the `readouts` vector
    pub active: usize,
}

impl Readout {
    /// Create a new `Readouts` state with no readouts
    pub const fn new() -> Self {
        Self::new_with_efuse(Vec::new())
    }

    /// Create a new `Readouts` state with no readouts and the given eFuse readouts
    pub const fn new_with_efuse(efuse_readouts: Vec<(String, String)>) -> Self {
        Self {
            efuse_readouts,
            readouts: Vec::new(),
            active: 0,
        }
    }

    /// Return `true` when all readouts are input
    pub fn is_ready(&self) -> bool {
        self.active == self.readouts.len()
    }
}

impl Default for Readout {
    fn default() -> Self {
        Self::new()
    }
}

/// The state of the model when the bundle is ready to be provisioned or in the process of being provisioned
/// (i.e. flashed and efused)
#[derive(Debug, Clone)]
pub struct Provision {
    /// The prepared bundle
    pub bundle: Bundle,
    /// Whether the bundle is being provisioned (flashed and efused)
    pub provisioning: bool,
}

/// The state of the model when processing a sub-task
#[derive(Debug)]
pub struct Processing {
    /// The title of the processing (e.g. "Preparing bundle", etc.)
    pub title: String,
    /// The status of the processing (e.g. "Loading", etc.)
    pub status: String,
    /// A counter helper for displaying a processing progress
    pub counter: Wrapping<usize>,
}

impl Processing {
    const fn empty() -> Self {
        Self {
            title: String::new(),
            status: String::new(),
            counter: Wrapping(0),
        }
    }

    /// Create a new `Preparing` state with empty status
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            status: String::new(),
            counter: Wrapping(0),
        }
    }
}

/// The state of the model when presenting a status message
///
/// The status message is either an error or a success message
#[derive(Debug)]
pub struct Status {
    pub title: String,
    pub message: String,
    pub error: bool,
}

impl Status {
    pub fn success(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(title, message, false)
    }

    pub fn error(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(title, message, true)
    }

    /// Create a new `Status` state with the given title and message
    pub fn new(title: impl Into<String>, message: impl Into<String>, error: bool) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            error,
        }
    }
}

#[derive(Debug)]
pub struct Logs {
    pub file: FileLogs,
    pub buffered: BufferedLogs,
}

impl Logs {
    pub const fn new(
        level: LevelFilter,
        layout: BufferedLogsLayout,
        width: u16,
        height: u16,
    ) -> Self {
        Self {
            file: FileLogs::new(level),
            buffered: BufferedLogs::new(level, 1000, layout, width, height),
        }
    }

    pub fn clear(&mut self) -> anyhow::Result<()> {
        self.file.start()?;
        //TODO self.buffered.clear();

        Ok(())
    }

    pub fn take(&mut self) -> Option<File> {
        self.file.grab()
    }
}

#[derive(Debug)]
pub struct FileLogs {
    pub level: LevelFilter,
    pub file: Option<File>,
}

impl FileLogs {
    pub const fn new(level: LevelFilter) -> Self {
        Self { level, file: None }
    }

    fn start(&mut self) -> anyhow::Result<()> {
        let log = tempfile()?;

        self.file = Some(log);

        Ok(())
    }

    pub fn grab(&mut self) -> Option<File> {
        self.file.take()
    }

    pub fn finish<'i, I, S>(mut log: File, summary: I) -> anyhow::Result<impl Read + Seek>
    where
        I: IntoIterator<Item = &'i (S, S)>,
        S: AsRef<str> + 'i,
    {
        let mut log_zip_file = tempfile()?;

        let mut log_zip = ZipWriter::new(&mut log_zip_file);
        log_zip.start_file("log.txt", FileOptions::<()>::default())?;

        log.flush()?;
        log.seek(SeekFrom::Start(0))?;

        std::io::copy(&mut log, &mut log_zip)?;

        drop(log);

        log_zip.start_file("log.csv", FileOptions::<()>::default())?;

        let mut csv = csv::WriterBuilder::new()
            .has_headers(true)
            .from_writer(&mut log_zip);

        csv.serialize(("Name", "Value"))?;

        for (name, value) in summary {
            csv.serialize((name.as_ref(), value.as_ref()))?;
        }

        csv.flush()?;

        drop(csv);
        drop(log_zip);

        log_zip_file.flush()?;
        log_zip_file.seek(SeekFrom::Start(0))?;

        Ok(log_zip_file)
    }

    pub fn log(&mut self, record: &Record) -> bool {
        if self.level >= record.level() {
            if let Some(out) = self.file.as_mut() {
                let message = format!(
                    "[{} {} {}] {}",
                    record.level(),
                    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
                    record.target(),
                    record.args()
                );

                let _ = out.write_all(message.as_bytes());
                let _ = out.write_all(b"\n");

                return true;
            }
        }

        false
    }

    pub fn flush(&mut self) {
        if let Some(out) = self.file.as_mut() {
            let _ = out.flush();
        }
    }
}

#[derive(Debug, Default, Copy, Clone, Eq, PartialEq, Hash)]
pub enum BufferedLogsLayout {
    Fullscreen,
    Hidden,
    #[default]
    Bottom,
}

impl BufferedLogsLayout {
    pub const fn toggle(&self) -> Self {
        match self {
            Self::Hidden => Self::Bottom,
            Self::Bottom => Self::Fullscreen,
            Self::Fullscreen => Self::Hidden,
        }
    }

    pub fn split(&self, area: Rect) -> (Rect, Rect) {
        const EMPTY_RECT: Rect = Rect::new(0, 0, 0, 0);

        match self {
            BufferedLogsLayout::Hidden => (area, EMPTY_RECT),
            BufferedLogsLayout::Bottom => {
                let logs_height = area.height.min(6);
                let main_height = (area.height as i32 - logs_height as i32).max(0) as u16;

                let main_area = Rect::new(area.x, area.y, area.width, main_height);
                let logs_area = Rect::new(area.x, area.y + main_height, area.width, logs_height);

                (main_area, logs_area)
            }
            BufferedLogsLayout::Fullscreen => (EMPTY_RECT, area),
        }
    }
}

#[derive(Debug)]
pub struct BufferedLogs {
    layout: BufferedLogsLayout,
    viewport: Rect,
    wrap: bool,
    level: LevelFilter,
    count: usize,
    // Use `ratatui::Line` directly in the model for performance reasons
    buffer: VecDeque<Line<'static>>,
    buffer_len: usize,
}

impl BufferedLogs {
    pub const fn new(
        level: LevelFilter,
        buffer_len: usize,
        view: BufferedLogsLayout,
        width: u16,
        height: u16,
    ) -> Self {
        Self {
            layout: view,
            viewport: Rect::new(0, 0, width, height),
            level,
            count: 0,
            wrap: true,
            buffer: VecDeque::new(),
            buffer_len,
        }
    }

    pub fn set_size(&mut self, width: u16, height: u16) {
        self.viewport.width = width;
        self.viewport.height = height;
    }

    pub const fn layout(&self) -> BufferedLogsLayout {
        self.layout
    }

    pub fn toggle_layout(&mut self) {
        self.layout = self.layout.toggle();

        if matches!(self.layout, BufferedLogsLayout::Fullscreen) {
            self.home_end_y(false);
        }
    }

    pub const fn is_wrap(&self) -> bool {
        self.wrap
    }

    pub fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
        self.viewport.x = 0;
    }

    pub fn log(&mut self, record: &Record) -> bool {
        if self.level >= record.level() {
            let no = self.count;

            let no = Span::from(format!("{:08} ", no)).on_white().black();

            let level = Span::from(format!("[{}] ", record.level().as_str()));
            let level = match record.level() {
                log::Level::Error => level.red().bold(),
                log::Level::Warn => level.yellow().bold(),
                log::Level::Info => level.green(),
                log::Level::Debug => level.blue(),
                log::Level::Trace => level.cyan(),
            };

            let lines = format!("{}", record.args());

            let mut iter = lines.lines();

            if let Some(first) = iter.next() {
                self.push(Line::from(vec![no, level, first.to_string().into()]));
            }

            for line in iter {
                self.push(Line::from(vec![line.to_string().into()]));
            }

            !matches!(self.layout, BufferedLogsLayout::Hidden)
        } else {
            false
        }
    }

    fn push(&mut self, line: Line<'static>) {
        if self.buffer.len() >= self.buffer_len {
            self.buffer.pop_front();
        }

        self.buffer.push_back(line);
        self.count += 1;
    }

    pub fn clear(&mut self) {
        self.viewport.x = 0;
        self.viewport.y = 0;

        self.buffer.clear();
    }

    pub fn home_end_x(&mut self, home: bool) {
        if self.wrap {
            self.viewport.x = 0;
            return;
        }

        if home {
            self.viewport.x = 0;
        } else {
            self.viewport.x = self.max_x() as _;
        }
    }

    pub fn scroll_x(&mut self, left: bool) {
        if self.wrap {
            self.viewport.x = 0;
            return;
        }

        if left {
            self.viewport.x = self.viewport.x.saturating_sub(1);
        } else {
            let max_x = self.max_x();

            self.viewport.x = (self.viewport.x as u32 + 1).min(max_x) as _;
        }
    }

    pub fn home_end_y(&mut self, home: bool) {
        if home {
            self.viewport.y = 0;
        } else {
            self.viewport.y = self.max_y() as _;
        }
    }

    pub fn scroll_y(&mut self, up: bool) {
        self.scroll_y_by(1, up);
    }

    pub fn page_scroll_y(&mut self, up: bool) {
        self.scroll_y_by(self.viewport.height, up);
    }

    pub fn scroll_y_by(&mut self, height: u16, up: bool) {
        if up {
            self.viewport.y = self.viewport.y.saturating_sub(height);
        } else {
            let max_y = self.max_y();

            self.viewport.y = (self.viewport.y as u32 + height as u32).min(max_y) as _;
        }
    }

    pub fn para(&self, scroll: bool, last_n_height: u16) -> Paragraph<'static> {
        let mut para = Paragraph::new(Text::from_iter(self.buffer.iter().cloned()));

        if self.wrap {
            para = para.wrap(Wrap { trim: true });
        }

        if scroll {
            match self.layout {
                BufferedLogsLayout::Fullscreen => para.scroll((self.viewport.y, self.viewport.x)),
                BufferedLogsLayout::Hidden => para,
                BufferedLogsLayout::Bottom => {
                    let lines_ct = para.line_count(self.viewport.width);
                    para.scroll(((lines_ct as i32 - last_n_height as i32).max(0) as u16, 0))
                }
            }
        } else {
            para
        }
    }

    fn max_y(&self) -> u32 {
        let para = self.para(false, 0);

        let lines_ct = para.line_count(self.viewport.width);

        (lines_ct as i32 - self.viewport.height as i32).max(0) as _
    }

    fn max_x(&self) -> u32 {
        let para = self.para(false, 0);

        let line_width = para.line_width();

        (line_width as i32 - self.viewport.width as i32).max(0) as _
    }
}
