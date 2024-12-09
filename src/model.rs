use core::cell::RefCell;
use core::num::Wrapping;
use std::collections::HashMap;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::signal::Signal;

use crate::bundle::Bundle;

extern crate alloc;

pub struct Model {
    state: Mutex<CriticalSectionRawMutex, RefCell<State>>, // TODO: Change to std::sync::Mutex?
    changed: Signal<CriticalSectionRawMutex, ()>,
}

impl Model {
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(RefCell::new(State::new())),
            changed: Signal::new(),
        }
    }

    pub fn get<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&State) -> R,
    {
        self.state.lock(|state| f(&*state.borrow()))
    }

    pub fn modify<F>(&self, f: F)
    where
        F: FnOnce(&mut State),
    {
        self.maybe_modify(|state| {
            f(state);
            true
        });
    }

    fn maybe_modify<F>(&self, f: F)
    where
        F: FnOnce(&mut State) -> bool,
    {
        self.state.lock(|state| {
            let mut state = state.borrow_mut();

            if f(&mut *state) {
                self.changed.signal(());
            }
        })
    }

    pub async fn wait_changed(&self) {
        self.changed.wait().await;
    }
}

#[derive(Debug)]
pub enum State {
    Preparing(Preparing),
    Empty(Empty),
    Prepared(Prepared),
    Provisioning(Provisioning),
    Provisioned(Provisioned),
}

impl State {
    pub const fn new() -> Self {
        Self::Preparing(Preparing {
            status: String::new(),
            counter: Wrapping(0),
        })
    }

    pub fn preparing(&mut self) -> &mut Preparing {
        if let Self::Preparing(preparing) = self {
            preparing
        } else {
            panic!("Unexpected state: {self:?}")
        }
    }

    pub fn prepared(&mut self) -> &mut Prepared {
        if let Self::Prepared(prepared) = self {
            prepared
        } else {
            panic!("Unexpected state: {self:?}")
        }
    }

    pub fn provisioning_mut(&mut self) -> &mut Provisioning {
        if let Self::Provisioning(provisioning) = self {
            provisioning
        } else {
            panic!("Unexpected state: {self:?}")
        }
    }

    pub fn provisioning(&self) -> &Provisioning {
        if let Self::Provisioning(provisioning) = self {
            provisioning
        } else {
            panic!("Unexpected state: {self:?}")
        }
    }

    pub fn provisioned(&mut self) -> &mut Provisioned {
        if let Self::Provisioned(provisioned) = self {
            provisioned
        } else {
            panic!("Unexpected state: {self:?}")
        }
    }
}

#[derive(Debug)]
pub struct Preparing {
    pub status: String,
    pub counter: Wrapping<usize>,
}

#[derive(Debug)]
pub struct Empty {}

#[derive(Debug)]
pub struct Prepared {
    pub bundle: Bundle,
}

#[derive(Debug, Clone)]
pub enum ProvisioningStatus {
    NotStarted,
    Pending,
    InProgress(u8),
    Done,
}

#[derive(Debug)]
pub struct Provisioning {
    pub bundle: Bundle,
    pub efuses_status: HashMap<String, ProvisioningStatus>,
}

#[derive(Debug)]
pub struct Provisioned {}
