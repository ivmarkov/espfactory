use core::cell::RefCell;
use core::num::Wrapping;

use alloc::sync::Arc;

use std::collections::HashMap;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::signal::Signal;

use crate::bundle::{Bundle, ProvisioningStatus};

extern crate alloc;

/// The model of the factory application
pub struct Model {
    /// The state of the model (i.e. awating readouts, displaying an error, preparing the bundle, flashing it etc. etc.)
    state: Mutex<CriticalSectionRawMutex, RefCell<State>>, // TODO: Change to std::sync::Mutex?
    /// A signal to notify that the model has changed
    /// Used to trigger redraws of the UI
    changed: Arc<Signal<CriticalSectionRawMutex, ()>>,
}

impl Model {
    /// Create a new model in the initial state (readouts)
    pub const fn new(changed: Arc<Signal<CriticalSectionRawMutex, ()>>) -> Self {
        Self {
            state: Mutex::new(RefCell::new(State::new())),
            changed,
        }
    }

    /// Get the current state of the model in the given closure
    pub fn get<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&State) -> R,
    {
        self.state.lock(|state| f(&state.borrow()))
    }

    /// Modify the state of the model by applying the given closure to it
    pub fn modify<F>(&self, f: F)
    where
        F: FnOnce(&mut State),
    {
        self.maybe_modify(|state| {
            f(state);
            true
        });
    }

    /// Maybe modify the state of the model by applying the given closure to it
    /// If the closure returns `true`, the model is considered to have changed and the `changed` signal is triggered
    pub fn maybe_modify<F>(&self, f: F)
    where
        F: FnOnce(&mut State) -> bool,
    {
        self.state.lock(|state| {
            let mut state = state.borrow_mut();

            if f(&mut state) {
                self.changed.signal(());
            }
        })
    }

    /// Wait for the model to change
    /// The UI is expected to call this method to wait for the model to change before redrawing
    pub async fn wait_changed(&self) {
        self.changed.wait().await;
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
        Self::Processing(Processing::new())
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
        if let Self::Provision(prepared) = self {
            prepared
        } else {
            panic!("Unexpected state: {self:?}")
        }
    }

    /// Get a mutable reference to the provision state
    /// Panics if the state is not `Provision`
    pub fn provision_mut(&mut self) -> &mut Provision {
        if let Self::Provision(prepared) = self {
            prepared
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
#[derive(Debug)]
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
#[derive(Debug)]
pub struct Provision {
    /// The prepared bundle
    pub bundle: Bundle,
    /// TBD
    pub efuses_status: HashMap<String, ProvisioningStatus>,
    /// Whether the bundle is being provisioned (flashed and efused)
    pub provisioning: bool,
}

/// The state of the model when processing a sub-task
#[derive(Debug)]
pub struct Processing {
    /// The status of the processing (e.g. "Loading", etc.)
    pub status: String,
    /// A counter helper for displaying a processing progress
    pub counter: Wrapping<usize>,
}

impl Processing {
    /// Create a new `Preparing` state with empty status
    pub const fn new() -> Self {
        Self {
            status: String::new(),
            counter: Wrapping(0),
        }
    }
}

impl Default for Processing {
    fn default() -> Self {
        Self::new()
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
