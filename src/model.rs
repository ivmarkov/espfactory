use core::cell::RefCell;
use core::num::Wrapping;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write as _};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::signal::Signal;

use log::{LevelFilter, Log as _, Record};

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
    changed: Signal<CriticalSectionRawMutex, ()>,
}

impl Model {
    /// Create a new model in the initial state (readouts)
    ///
    /// Arguments:
    /// - `log_level`: The log level of the model (for both file logging as well as on-screen logging)
    /// - `no_ui`: When `true` the interactive UI is disabled and the console logger is active
    /// - `log_buffer_len`: The maximum number of log lines to keep in the on-screen logs buffer
    /// - `width`: The initial width of the screen (necessary for proper paging in the on-screen logs)
    /// - `height`: The initial height of the screen (necessary for proper paging in the on-screen logs)
    pub const fn new(
        log_level: LevelFilter,
        no_ui: bool,
        log_buffer_len: usize,
        width: u16,
        height: u16,
    ) -> Self {
        Self {
            state: Mutex::new(RefCell::new(ModelInner::new(
                log_level,
                no_ui,
                log_buffer_len,
                width,
                height,
            ))),
            changed: Signal::new(),
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

    /// Modify the state of the model by applying the given closure to it
    pub fn modify<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut ModelInner) -> R,
    {
        self.access_mut(|inner| (f(inner), true))
    }

    /// Wait for the model to change
    /// The UI is expected to call this method to wait for the model to change before redrawing
    pub async fn wait_changed(&self) {
        self.changed.wait().await;
    }
}

/// The inner state of the model, accessible in the closures passed to
/// `Model::access`, `Model::access_mut` and `Model::modify`.
#[derive(Debug)]
pub struct ModelInner {
    /// The state of the model (i.e. awating readouts, displaying an error, preparing the bundle, flashing it etc. etc.)
    pub state: State,
    /// The logs' state of the model (i.e. whether the logs are active, the position inside the logs etc. etc.)
    pub logs: Logs,
}

impl ModelInner {
    /// Create a new model in the initial state (readouts)
    ///
    /// Arguments:
    /// - `log_level`: The log level of the model (for both file logging as well as on-screen logging)
    /// - `no_ui`: When `true` the interactive UI is disabled and the console logger is active
    /// - `log_buffer_len`: The maximum number of log lines to keep in the on-screen logs buffer
    /// - `width`: The initial width of the screen (necessary for proper paging in the on-screen logs)
    /// - `height`: The initial height of the screen (necessary for proper paging in the on-screen logs)
    pub const fn new(
        log_level: LevelFilter,
        no_ui: bool,
        log_buffer_len: usize,
        width: u16,
        height: u16,
    ) -> Self {
        Self {
            state: State::new(),
            logs: Logs::new(
                log_level,
                no_ui,
                log_buffer_len,
                BufferedLogsLayout::Bottom,
                width,
                height,
            ),
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
        Self {
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
    /// The readouts (manual and eFuse IDs) to display
    /// Each readout is a tuple of the readout key and its stringified value
    pub readouts: Vec<(String, String)>,
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
    /// The title of the status message
    pub title: String,
    /// The message of the status
    pub message: String,
    /// Whether the status is an error
    pub error: bool,
}

impl Status {
    /// Create a new "success" `Status` state with the given title and message
    pub fn success(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(title, message, false)
    }

    /// Create a new "error" `Status` state with the given title and message
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

/// The logs of the model
#[derive(Debug)]
pub struct Logs {
    /// File logs
    pub file: FileLogs,
    /// Buffered (on-screen) logs
    pub buffered: BufferedLogs,
}

impl Logs {
    /// Create a new `Logs` state with the given log level, layout, width and height
    ///
    /// Arguments:
    /// - `level`: The log level of the model (for both file logging as well as on-screen logging)
    /// - `no_ui`: When `true` the interactive UI is disabled and the console logger is active
    /// - `buffer_len`: The maximum number of log lines to keep in the on-screen logs buffer
    /// - `layout`: The initial layout of the on-screen logs (i.e. fullscreen, hidden or bottom)
    /// - `width`: The initial width of the screen (necessary for proper paging in the on-screen logs)
    /// - `height`: The initial height of the screen (necessary for proper paging in the on-screen logs)
    pub const fn new(
        level: LevelFilter,
        no_ui: bool,
        buffer_len: usize,
        layout: BufferedLogsLayout,
        width: u16,
        height: u16,
    ) -> Self {
        Self {
            file: FileLogs::new(level, no_ui),
            buffered: BufferedLogs::new(
                if level as usize <= LevelFilter::Info as usize {
                    level
                } else {
                    LevelFilter::Info
                },
                buffer_len,
                layout,
                width,
                height,
            ),
        }
    }

    /// Clear the logs state
    ///
    /// To be called when a new PCB is to be provisioned
    pub fn clear(&mut self) -> anyhow::Result<()> {
        self.file.start()?;
        self.buffered.clear();

        Ok(())
    }

    /// Take the file of the file logs
    ///
    /// To be called at the end of the PCB provisioning, when file logs are to be uploaded
    pub fn take(&mut self) -> Option<File> {
        self.file.grab()
    }
}

/// The file logs of the model
#[derive(Debug)]
pub struct FileLogs {
    /// The log level of the file logs
    level: LevelFilter,
    /// When `true` the interactive UI is disabled and the console logger below is active
    no_ui: bool,
    /// The file to write the logs to
    file: Option<File>,
    /// The optional console logger in case the interactive UI is disabled
    console: Option<env_logger::Logger>,
}

impl FileLogs {
    /// Create a new `FileLogs` state with no log file
    ///
    /// Arguments:
    /// - `level`: The log level of the model (for file logging)
    pub const fn new(level: LevelFilter, no_ui: bool) -> Self {
        Self {
            level,
            no_ui,
            file: None,
            console: None,
        }
    }

    /// Start the file logs
    fn start(&mut self) -> anyhow::Result<()> {
        let log = tempfile()?;

        self.file = Some(log);

        if self.no_ui && self.console.is_none() {
            self.console = Some(
                env_logger::Builder::new()
                    .format_timestamp(None)
                    .format_target(false)
                    .filter_level(self.level)
                    .build(),
            );
        }

        Ok(())
    }

    /// Take the file of the file logs, if any
    pub fn grab(&mut self) -> Option<File> {
        self.file.take()
    }

    /// Utility to finish the file logs
    ///
    /// Finishing the file logs means flushing the logs to the file and creating
    /// a ZIP file with the logs and a small summary csv
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

    /// Log a record to the file logs
    pub fn log(&mut self, record: &Record) -> bool {
        let mut logged = false;

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

                logged |= true;
            }

            if let Some(console) = self.console.as_mut() {
                console.log(record);

                logged |= true;
            }
        }

        logged
    }

    /// Flush the file logs
    pub fn flush(&mut self) {
        if let Some(out) = self.file.as_mut() {
            let _ = out.flush();
        }
    }
}

/// The layout of the on-screen logs
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq, Hash)]
pub enum BufferedLogsLayout {
    /// The logs are fullscreen
    Fullscreen,
    /// The logs are hidden
    Hidden,
    /// The logs occupy N fixed lines at the bottom of the screen
    #[default]
    Bottom,
}

impl BufferedLogsLayout {
    /// Toggle the layout between hidden, bottom and fullscreen
    pub const fn toggle(&self) -> Self {
        match self {
            Self::Hidden => Self::Bottom,
            Self::Bottom => Self::Fullscreen,
            Self::Fullscreen => Self::Hidden,
        }
    }

    /// Utility to calculate the split of the screen between the main area and the logs area
    /// depending on the layout
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

/// The buffered (on-screen) logs of the model
#[derive(Debug)]
pub struct BufferedLogs {
    /// The layout of the on-screen logs
    layout: BufferedLogsLayout,
    /// The viewport of the on-screen logs (current position inside the logs buffer as well as current
    /// screen size known to the model - for correct paging through the logs)
    viewport: Rect,
    /// Whether the logs are wrapped when displayed
    wrap: bool,
    /// The log level of the on-screen logs
    level: LevelFilter,
    /// The number of log lines sent to the on-screen logs buffer from the beginning of the program
    /// (used to number the log lines)
    count: usize,
    /// The buffer of the on-screen logs. Keeps the last N log lines
    /// Uses `ratatui::Line` directly in the model for performance reasons
    buffer: VecDeque<Line<'static>>,
    /// The maximum number of log lines to keep in the buffer
    buffer_len: usize,
}

impl BufferedLogs {
    /// Create a new `BufferedLogs` state with the given log level, layout, width and height
    ///
    /// Arguments:
    /// - `level`: The log level of the model (for on-screen logging)
    /// - `buffer_len`: The maximum number of log lines to keep in the buffer
    /// - `view`: The initial layout of the on-screen logs (i.e. fullscreen, hidden or bottom)
    /// - `width`: The initial width of the screen (necessary for proper paging in the on-screen logs)
    /// - `height`: The initial height of the screen (necessary for proper paging in the on-screen logs)
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

    /// Ipdate the model with the last know screen size (for proper paging through the logs)
    pub fn set_size(&mut self, width: u16, height: u16) {
        self.viewport.width = width;
        self.viewport.height = height;
    }

    /// Get the layout of the on-screen logs
    pub const fn layout(&self) -> BufferedLogsLayout {
        self.layout
    }

    /// Toggle the layout of the on-screen logs between hidden, bottom and fullscreen
    pub fn toggle_layout(&mut self) {
        self.layout = self.layout.toggle();

        if matches!(self.layout, BufferedLogsLayout::Fullscreen) {
            self.home_end_y(false);
        }
    }

    /// Get the wrap setting of the on-screen logs
    pub const fn is_wrap(&self) -> bool {
        self.wrap
    }

    /// Toggle the wrap setting of the on-screen logs
    pub fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
        self.viewport.x = 0;
    }

    /// Log a record to the on-screen logs
    pub fn log(&mut self, record: &Record) -> bool {
        if self.level >= record.level() && self.buffer_len > 0 {
            let no = self.count + 1;

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

    /// Push a log line to the on-screen logs buffer, removing the oldest line if necessary
    fn push(&mut self, line: Line<'static>) {
        if self.buffer.len() >= self.buffer_len {
            self.buffer.pop_front();
        }

        self.buffer.push_back(line);
        self.count += 1;
    }

    /// Clear the on-screen logs buffer
    pub fn clear(&mut self) {
        self.viewport.x = 0;
        self.viewport.y = 0;

        self.buffer.clear();
        self.count = 0;
    }

    /// Move the viewport to the beginning or the end of the on-screen logs by the X axis
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

    /// Scroll the viewport by one column to the left or to the right
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

    /// Move the viewport to the beginning or the end of the on-screen logs by the Y axis
    pub fn home_end_y(&mut self, home: bool) {
        if home {
            self.viewport.y = 0;
        } else {
            self.viewport.y = self.max_y() as _;
        }
    }

    /// Scroll the viewport by one line up or down
    pub fn scroll_y(&mut self, up: bool) {
        self.scroll_y_by(1, up);
    }

    /// Scroll the viewport by one page up or down
    pub fn page_scroll_y(&mut self, up: bool) {
        self.scroll_y_by((self.viewport.height as i32 - 1).max(0) as _, up);
    }

    /// Scroll the viewport by the given height up or down
    pub fn scroll_y_by(&mut self, height: u16, up: bool) {
        if up {
            self.viewport.y = self.viewport.y.saturating_sub(height);
        } else {
            let max_y = self.max_y();

            self.viewport.y = (self.viewport.y as u32 + height as u32).min(max_y) as _;
        }
    }

    /// Get the on-screen logs as a `Paragraph` widget, ready for rendering
    pub fn para(&self, scroll: bool, last_n_height: u16) -> Paragraph<'static> {
        let mut para = Paragraph::new(Text::from_iter(self.buffer.iter().cloned()));

        if self.wrap {
            para = para.wrap(Wrap { trim: false });
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

    /// Get the maximum meaningful Y position of the viewport
    fn max_y(&self) -> u32 {
        let para = self.para(false, 0);

        let lines_ct = para.line_count(self.viewport.width);

        (lines_ct as i32 - (self.viewport.height as i32 - 1).max(0)).max(0) as _
    }

    /// Get the maximum meaningful X position of the viewport
    fn max_x(&self) -> u32 {
        let para = self.para(false, 0);

        let line_width = para.line_width();

        (line_width as i32 - self.viewport.width as i32).max(0) as _
    }
}
