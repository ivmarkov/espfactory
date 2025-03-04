use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use embassy_futures::select::{select, Either};
use embassy_sync::signal::Signal;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, RawMutex};
use embassy_sync::channel::Channel;

use crate::input::{
    LogInput, LogInputOutcome, TaskConfirmationOutcome, TaskInput, TaskInputOutcome,
};
use crate::model::{BufferedLogsLayout, Model};

extern crate alloc;

/// A helper for procressing input events from the terminal
pub struct Input<'a> {
    model: &'a Model,
    input_changed_main: Signal<CriticalSectionRawMutex, ()>,
    input_changed_log: Signal<CriticalSectionRawMutex, ()>,
    pump: EventsPump,
}

impl<'a> Input<'a> {
    const PREV: (KeyModifiers, KeyCode) = (KeyModifiers::empty(), KeyCode::Esc);
    const NEXT: (KeyModifiers, KeyCode) = (KeyModifiers::empty(), KeyCode::Enter);
    const QUIT: (KeyModifiers, KeyCode) = (KeyModifiers::ALT, KeyCode::Char('q'));
    const SKIP: (KeyModifiers, KeyCode) = (KeyModifiers::ALT, KeyCode::Char('i'));

    const UP: (KeyModifiers, KeyCode) = (KeyModifiers::empty(), KeyCode::Up);
    const DOWN: (KeyModifiers, KeyCode) = (KeyModifiers::empty(), KeyCode::Down);
    const LEFT: (KeyModifiers, KeyCode) = (KeyModifiers::empty(), KeyCode::Left);
    const RIGHT: (KeyModifiers, KeyCode) = (KeyModifiers::empty(), KeyCode::Right);
    const PAGE_UP: (KeyModifiers, KeyCode) = (KeyModifiers::empty(), KeyCode::PageUp);
    const PAGE_DOWN: (KeyModifiers, KeyCode) = (KeyModifiers::empty(), KeyCode::PageDown);
    const HOME: (KeyModifiers, KeyCode) = (KeyModifiers::empty(), KeyCode::Home);
    const END: (KeyModifiers, KeyCode) = (KeyModifiers::empty(), KeyCode::End);
    const CTL_HOME: (KeyModifiers, KeyCode) = (KeyModifiers::CONTROL, KeyCode::Home);
    const CTL_END: (KeyModifiers, KeyCode) = (KeyModifiers::CONTROL, KeyCode::End);

    const TOGGLE_LOG: (KeyModifiers, KeyCode) = (KeyModifiers::ALT, KeyCode::Char('l'));
    const WRAP_LOG: (KeyModifiers, KeyCode) = (KeyModifiers::ALT, KeyCode::Char('w'));

    /// Creates a new `Input` instance with the given model
    pub fn new(model: &'a Model) -> Self {
        Self {
            model,
            input_changed_main: Signal::new(),
            input_changed_log: Signal::new(),
            pump: EventsPump::new(),
        }
    }

    /// Gets the next key press event in case the main input is active
    /// Waits until the main input is activated otherwise
    async fn get_main_input(&self) -> KeyEvent {
        self.get(true, &self.input_changed_main).await
    }

    /// Gets the next key press event in case the log input is active
    /// Waits until the log input is activated otherwise
    async fn get_log_input(&self) -> KeyEvent {
        self.get(false, &self.input_changed_log).await
    }

    /// Gets the next key press event either for the main input or for the log input
    /// depending on the value of the `main` parameter
    async fn get(&self, main: bool, signal: &Signal<impl RawMutex, ()>) -> KeyEvent {
        loop {
            if self.model.access(|inner| {
                main != matches!(inner.logs.buffered.layout(), BufferedLogsLayout::Fullscreen)
            }) {
                if let Either::Second(key) = select(signal.wait(), self.get_any()).await {
                    return key;
                }
            } else {
                signal.wait().await;
            }
        }
    }

    /// Gets the next key press event
    async fn get_any(&self) -> KeyEvent {
        self.pump.start();

        loop {
            match self.pump.state.event.receive().await {
                // It's important to check that the event is a key press event as
                // crossterm also emits key release and repeat events on Windows.
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if Self::key_m(&key) == Self::TOGGLE_LOG || Self::key_m(&key) == Self::WRAP_LOG
                    {
                        self.model.modify(|inner| {
                            let buffered = &mut inner.logs.buffered;

                            if Self::key_m(&key) == Self::TOGGLE_LOG {
                                buffered.toggle_layout();
                            } else {
                                buffered.toggle_wrap();
                            }

                            self.input_changed_main.signal(());
                            self.input_changed_log.signal(());
                        });
                    } else {
                        return key;
                    }
                }
                // Fake a dirty model to force redraw on resize
                Event::Resize(width, height) => self.model.modify(|inner| {
                    let buffered = &mut inner.logs.buffered;
                    buffered.set_size(width, height);
                }),
                _ => {}
            }
        }
    }

    pub fn key_m(event: &KeyEvent) -> (KeyModifiers, KeyCode) {
        (event.modifiers, event.code)
    }
}

impl TaskInput for &Input<'_> {
    /// Waits for the user to:
    /// - Go back to the previous step with `Esc`
    /// - or to quit the application with `q`
    async fn wait_cancel(&mut self) -> TaskConfirmationOutcome {
        loop {
            match Input::key_m(&self.get_main_input().await) {
                Input::PREV => break TaskConfirmationOutcome::Canceled,
                Input::QUIT => break TaskConfirmationOutcome::Quit,
                _ => (),
            }
        }
    }

    async fn confirm(&mut self, _label: &str) -> TaskConfirmationOutcome {
        loop {
            match Input::key_m(&self.get_main_input().await) {
                Input::NEXT => break TaskConfirmationOutcome::Confirmed,
                Input::PREV => break TaskConfirmationOutcome::Canceled,
                Input::QUIT => break TaskConfirmationOutcome::Quit,
                _ => (),
            }
        }
    }

    async fn confirm_or_skip(&mut self, _label: &str) -> TaskConfirmationOutcome {
        loop {
            match Input::key_m(&self.get_main_input().await) {
                Input::NEXT => break TaskConfirmationOutcome::Confirmed,
                Input::PREV => break TaskConfirmationOutcome::Canceled,
                Input::QUIT => break TaskConfirmationOutcome::Quit,
                Input::SKIP => break TaskConfirmationOutcome::Skipped,
                _ => (),
            }
        }
    }

    async fn input(&mut self, _label: &str, current: &str) -> TaskInputOutcome {
        let mut current: String = current.to_string();

        loop {
            let key = self.get_main_input().await;

            match Input::key_m(&key) {
                Input::NEXT => {
                    if !current.is_empty() {
                        return TaskInputOutcome::Done(current);
                    }
                }
                Input::PREV => {
                    return TaskInputOutcome::StartOver;
                }
                Input::QUIT => {
                    return TaskInputOutcome::Quit;
                }
                (modifiers, code) => {
                    if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT {
                        match code {
                            KeyCode::Backspace => {
                                if current.pop().is_some() {
                                    return TaskInputOutcome::Modified(current);
                                }
                            }
                            KeyCode::Char(ch) => {
                                current.push(ch);
                                return TaskInputOutcome::Modified(current);
                            }
                            _ => (),
                        }
                    }
                }
            }
        }
    }

    async fn swallow(&mut self) -> ! {
        loop {
            self.get_main_input().await;
        }
    }
}

impl LogInput for &Input<'_> {
    async fn get(&mut self) -> LogInputOutcome {
        loop {
            match Input::key_m(&self.get_log_input().await) {
                Input::CTL_HOME => break LogInputOutcome::LogHome,
                Input::CTL_END => break LogInputOutcome::LogEnd,
                Input::UP => break LogInputOutcome::Up,
                Input::DOWN => break LogInputOutcome::Down,
                Input::LEFT => break LogInputOutcome::Left,
                Input::RIGHT => break LogInputOutcome::Right,
                Input::PAGE_UP => break LogInputOutcome::PgUp,
                Input::PAGE_DOWN => break LogInputOutcome::PgDown,
                Input::HOME => break LogInputOutcome::Home,
                Input::END => break LogInputOutcome::End,
                _ => (),
            }
        }
    }
}

/// A helper for processing input events from the terminal using async code,
/// by moving the blocking code that poll for events to a dedicated thread
struct EventsPump {
    state: Arc<EventsPumpState>,
    thread_join: std::sync::Mutex<Option<std::thread::JoinHandle<anyhow::Result<()>>>>,
}

impl EventsPump {
    /// Creates a new `EventsPump` instance
    fn new() -> Self {
        Self {
            state: Arc::new(EventsPumpState::new()),
            thread_join: std::sync::Mutex::new(None),
        }
    }

    /// Starts the event pump thread if not started yet
    fn start(&self) {
        let mut thread_join = self.thread_join.lock().unwrap();

        if thread_join.is_none() {
            let state = self.state.clone();

            *thread_join = Some(std::thread::spawn(move || state.pump_loop()));
        }
    }
}

impl Drop for EventsPump {
    fn drop(&mut self) {
        self.state.quit.store(true, Ordering::SeqCst);

        if let Some(thread_join) = self.thread_join.lock().unwrap().take() {
            thread_join.join().unwrap().unwrap();
        }
    }
}

/// The state of the event pump
struct EventsPumpState {
    /// The channel for getting events from the terminal
    /// Up to 10 events can be buffered
    event: Channel<CriticalSectionRawMutex, Event, 10>,
    /// A flag indicating whether the event pump thread should quit
    /// This flag is used to signal the event pump thread to quit when dropping the `EventsPump` instance
    quit: AtomicBool,
}

impl EventsPumpState {
    /// Creates a new `EventsPumpState` instance
    const fn new() -> Self {
        Self {
            event: Channel::new(),
            quit: AtomicBool::new(false),
        }
    }

    /// The main event pump loop (to be called from the event pump thread)
    fn pump_loop(&self) -> anyhow::Result<()> {
        while !self.quit.load(Ordering::SeqCst) {
            if event::poll(core::time::Duration::from_millis(100))? {
                futures_lite::future::block_on(self.event.send(event::read()?));
            }
        }

        Ok(())
    }
}
