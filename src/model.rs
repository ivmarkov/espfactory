use core::cell::RefCell;
use core::num::Wrapping;

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
    changed: Signal<CriticalSectionRawMutex, ()>,
}

impl Model {
    /// Create a new model in the initial state (readouts)
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(RefCell::new(State::new(Readouts::new()))),
            changed: Signal::new(),
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
    /// The model is awaiting readouts
    Readouts(Readouts),
    /// The model is preparing the bundle
    Preparing(Preparing),
    /// The model failed to prepare the bundle (error display)
    PreparingFailed(PreparingFailed),
    /// The model has prepared the bundle
    Prepared(Prepared),
    /// The model is provisioning the bundle
    Provisioning(Provisioning),
    /// The model has provisioned the bundle (flashed and possibly efused)
    Provisioned(Provisioned),
}

impl State {
    /// Create a new state in the `Readouts` state
    pub const fn new(readouts: Readouts) -> Self {
        Self::Readouts(readouts)
    }

    /// Get a reference to the readouts from the state
    /// Panics if the state is not `Readouts`
    pub fn readouts(&self) -> &Readouts {
        if let Self::Readouts(readouts) = self {
            readouts
        } else {
            panic!("Unexpected state: {self:?}")
        }
    }

    /// Get a mutable reference to the readouts from the state
    /// Panics if the state is not `Readouts`
    pub fn readouts_mut(&mut self) -> &mut Readouts {
        if let Self::Readouts(readouts) = self {
            readouts
        } else {
            panic!("Unexpected state: {self:?}")
        }
    }

    /// Get a mutable reference to the preparing state
    /// Panics if the state is not `Preparing`
    pub fn preparing_mut(&mut self) -> &mut Preparing {
        if let Self::Preparing(preparing) = self {
            preparing
        } else {
            panic!("Unexpected state: {self:?}")
        }
    }

    /// Get a mutable reference to the prepared state
    /// Panics if the state is not `Prepared`
    pub fn prepared_mut(&mut self) -> &mut Prepared {
        if let Self::Prepared(prepared) = self {
            prepared
        } else {
            panic!("Unexpected state: {self:?}")
        }
    }

    /// Get a reference to the provisioning state
    /// Panics if the state is not `Provisioning`
    pub fn provisioning(&self) -> &Provisioning {
        if let Self::Provisioning(provisioning) = self {
            provisioning
        } else {
            panic!("Unexpected state: {self:?}")
        }
    }

    /// Get a mutable reference to the provisioning state
    /// Panics if the state is not `Provisioning`
    pub fn provisioning_mut(&mut self) -> &mut Provisioning {
        if let Self::Provisioning(provisioning) = self {
            provisioning
        } else {
            panic!("Unexpected state: {self:?}")
        }
    }

    /// Get a mutable reference to the provisioned state
    /// Panics if the state is not `Provisioned`
    pub fn provisioned_mut(&mut self) -> &mut Provisioned {
        if let Self::Provisioned(provisioned) = self {
            provisioned
        } else {
            panic!("Unexpected state: {self:?}")
        }
    }
}

/// The readouts state of the model
#[derive(Debug)]
pub struct Readouts {
    /// The readouts to display and input
    /// Each readout is a tuple of the readout name and the readout value
    pub readouts: Vec<(String, String)>,
    /// The index of the active readout
    /// Used to indicate which readout is currently being input
    /// When all readouts are input, this is equal to the length of the `readouts` vector
    pub active: usize,
}

impl Readouts {
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

impl Default for Readouts {
    fn default() -> Self {
        Self::new()
    }
}

/// The state of the model when preparing the bundle
#[derive(Debug)]
pub struct Preparing {
    /// The status of the preparation (e.g. "Loading", etc.)
    pub status: String,
    /// A counter helper for displaying a preparation progress
    pub counter: Wrapping<usize>,
}

impl Preparing {
    /// Create a new `Preparing` state with empty status
    pub const fn new() -> Self {
        Self {
            status: String::new(),
            counter: Wrapping(0),
        }
    }
}

impl Default for Preparing {
    fn default() -> Self {
        Self::new()
    }
}

/// The state of the model when preparing the bundle had failed TBD
#[derive(Debug)]
pub struct PreparingFailed {}

/// The state of the model when the bundle is prepared
/// The bundle is ready to be flashed and efused
#[derive(Debug)]
pub struct Prepared {
    /// The prepared bundle
    pub bundle: Bundle,
}

/// The state of the model when the bundle is being provisioned (flashed and efused)
#[derive(Debug)]
pub struct Provisioning {
    /// The prepared bundle
    pub bundle: Bundle,
    /// TBD
    pub efuses_status: HashMap<String, ProvisioningStatus>,
}

/// The state of the model after the bundle was provisioned (flashed and efused)
#[derive(Debug)]
pub struct Provisioned {
    /// The already-provisioned bundle name
    pub bundle_name: String,
}
