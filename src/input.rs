use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;

use crate::model::Model;

extern crate alloc;

pub struct Input<'a> {
    model: &'a Model,
    pump: EventsPump,
}

impl<'a> Input<'a> {
    pub fn new(model: &'a Model) -> Self {
        Self {
            model,
            pump: EventsPump::new(),
        }
    }

    pub async fn wait_quit(&self) -> anyhow::Result<()> {
        loop {
            if self.get().await? == KeyCode::Char('q') {
                return Ok(());
            }
        }
    }

    pub async fn wait_quit_or(&self, code: KeyCode) -> anyhow::Result<bool> {
        loop {
            let got = self.get().await?;

            if got == code {
                return Ok(true);
            } else if got == KeyCode::Char('q') {
                return Ok(false);
            }
        }
    }

    #[allow(unused)]
    pub async fn swallow(&self) -> anyhow::Result<()> {
        loop {
            self.get().await?;
        }
    }

    pub async fn get(&self) -> anyhow::Result<KeyCode> {
        self.pump.start()?;

        loop {
            match self.pump.state.event.receive().await {
                // It's important to check that the event is a key press event as
                // crossterm also emits key release and repeat events on Windows.
                Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                    return Ok(key_event.code);
                }
                // Fake a dirty model to force redraw on resize
                Event::Resize(_, _) => self.model.modify(|_| {}),
                _ => {}
            }
        }
    }
}

struct EventsPump {
    state: Arc<EventsPumpState>,
    thread_join: std::sync::Mutex<Option<std::thread::JoinHandle<anyhow::Result<()>>>>,
}

impl EventsPump {
    fn new() -> Self {
        Self {
            state: Arc::new(EventsPumpState::new()),
            thread_join: std::sync::Mutex::new(None),
        }
    }

    fn start(&self) -> anyhow::Result<()> {
        let mut thread_join = self.thread_join.lock().unwrap();

        if thread_join.is_none() {
            let state = self.state.clone();

            *thread_join = Some(std::thread::spawn(move || state.pump_loop()));
        }

        Ok(())
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

struct EventsPumpState {
    event: Channel<CriticalSectionRawMutex, Event, 1>,
    quit: AtomicBool,
}

impl EventsPumpState {
    const fn new() -> Self {
        Self {
            event: Channel::new(),
            quit: AtomicBool::new(false),
        }
    }

    fn pump_loop(&self) -> anyhow::Result<()> {
        while !self.quit.load(Ordering::SeqCst) {
            if event::poll(core::time::Duration::from_millis(100))? {
                futures_lite::future::block_on(self.event.send(event::read()?));
            }
        }

        Ok(())
    }
}
