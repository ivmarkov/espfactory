use alloc::sync::Arc;
use embassy_futures::select::{select, Either};
use embassy_sync::signal::Signal;
use core::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, RawMutex};
use embassy_sync::channel::Channel;

use crate::model::Model;

extern crate alloc;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ConfirmOutcome {
    Confirmed,
    Canceled,
    Quit,
}

/// A helper for procressing input events from the terminal
pub struct Input<'a> {
    model: &'a Model,
    pump: EventsPump,
}

impl<'a> Input<'a> {
    pub const PREV: (KeyModifiers, KeyCode) = (KeyModifiers::empty(), KeyCode::Esc);
    pub const NEXT: (KeyModifiers, KeyCode) = (KeyModifiers::empty(), KeyCode::Enter);
    pub const QUIT: (KeyModifiers, KeyCode) = (KeyModifiers::ALT, KeyCode::Char('q'));
    
    pub const UP: (KeyModifiers, KeyCode) = (KeyModifiers::empty(), KeyCode::Up);
    pub const DOWN: (KeyModifiers, KeyCode) = (KeyModifiers::empty(), KeyCode::Down);
    pub const LEFT: (KeyModifiers, KeyCode) = (KeyModifiers::ALT, KeyCode::Left);
    pub const RIGHT: (KeyModifiers, KeyCode) = (KeyModifiers::ALT, KeyCode::Right);
    pub const PAGE_UP: (KeyModifiers, KeyCode) = (KeyModifiers::empty(), KeyCode::PageUp);
    pub const PAGE_DOWN: (KeyModifiers, KeyCode) = (KeyModifiers::empty(), KeyCode::PageDown);
    pub const HOME: (KeyModifiers, KeyCode) = (KeyModifiers::empty(), KeyCode::PageUp);
    pub const END: (KeyModifiers, KeyCode) = (KeyModifiers::empty(), KeyCode::PageDown);
    pub const CTL_HOME: (KeyModifiers, KeyCode) = (KeyModifiers::CONTROL, KeyCode::PageUp);
    pub const CTL_END: (KeyModifiers, KeyCode) = (KeyModifiers::CONTROL, KeyCode::PageDown);
    
    pub const TOGGLE_LOG: (KeyModifiers, KeyCode) = (KeyModifiers::ALT, KeyCode::Char('l'));
    
    /// Creates a new `Input` instance with the given model
    ///
    /// The model is necessary only so that the input can automatically trigger redraws on terminal resize events
    pub fn new(model: &'a Model) -> Self {
        Self {
            model,
            pump: EventsPump::new(),
        }
    }

    /// Waits for the user to:
    /// - Go back to the previous step with `Esc`
    /// - or to quit the application with `q`
    pub async fn wait_cancel(&self) -> ConfirmOutcome {
        loop {
            match Self::key_m(&self.get_main().await) {
                Self::PREV => break ConfirmOutcome::Canceled,
                Self::QUIT => break ConfirmOutcome::Quit,
                _ => (),
            }
        }
    }

    /// Waits for the user to:
    /// - Confirm the next step with `Enter`
    /// - or to go back to the previous step with `Esc`
    /// - or to quit the application with `q`
    pub async fn wait_confirm(&self) -> ConfirmOutcome {
        loop {
            match Self::key_m(&self.get_main().await) {
                Self::NEXT => break ConfirmOutcome::Confirmed,
                Self::PREV => break ConfirmOutcome::Canceled,
                Self::QUIT => break ConfirmOutcome::Quit,
                _ => (),
            }
        }
    }

    /// Swallows all key presses
    pub async fn swallow(&self) -> ! {
        loop {
            self.get_main().await;
        }
    }

    /// Gets the next key press event
    pub async fn get_input(&self) -> KeyEvent {
        self.get_main().await
    }

    pub async fn get_main(&self) -> KeyEvent {
        self.get0(true, &self.model.change_main_input).await
    }

    pub async fn get_log(&self) -> KeyEvent {
        self.get0(false, &self.model.change_log_input).await
    }

    async fn get0(&self, main: bool, signal: &Signal<impl RawMutex, ()>) -> KeyEvent {
        loop {
            if self.model.access(|inner| main == inner.fullscreen_logs.position.is_some()) {
                if let Either::Second(key) = select(signal.wait(), self.get_inner()).await {
                    return key.0;
                }
            } else {
                signal.wait().await;
            }
        }
    }

    /// Gets the next key press event
    async fn get_inner(&self) -> (KeyEvent, bool) {
        self.pump.start();

        loop {
            match self.pump.state.event.receive().await {
                // It's important to check that the event is a key press event as
                // crossterm also emits key release and repeat events on Windows.
                Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                    let mut key = None;

                    self.model.access_mut(|inner| {
                        let log_active = inner.fullscreen_logs.position.is_some();

                        if Self::key_m(&key_event) == Self::TOGGLE_LOG {
                            if log_active {
                                inner.fullscreen_logs.position = None;
                            } else {
                                inner.fullscreen_logs.position = Some(0);
                            }

                            true
                        } else {
                            key = Some((key_event, log_active));

                            false
                        }
                    });

                    if let Some((key, log_active)) = key {
                        return (key, log_active);
                    }
                }
                // Fake a dirty model to force redraw on resize
                Event::Resize(_, _) => self.model.modify(|_| {}),
                _ => {}
            }
        }
    }

    pub fn key_m(event: &KeyEvent) -> (KeyModifiers, KeyCode) {
        (event.modifiers, event.code)
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
