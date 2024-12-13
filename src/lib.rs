#![allow(async_fn_in_trait)]

use std::path::Path;

use alloc::sync::Arc;

use embassy_futures::select::select;

use input::Input;
use model::Model;
use serde::Deserialize;
use task::Task;
use utils::futures::Coalesce;
use view::View;

extern crate alloc;

mod bundle;
mod flash;
mod input;
pub mod loader;
mod model;
mod task;
mod utils;
mod view;

/// The configuration of the factory
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
pub struct Config {
    /// The serial port to use for communication with the device
    /// If not provided, the first available port where an ESP chip is
    /// detected will be used
    pub port: Option<String>,
    /// The method used to identify the bundle to be loaded
    pub bundle_identification: BundleIdentification,
    /// Whether to render a UI for reading the test jig ID
    /// The test jig Id is only read and used for logging purposes
    pub test_jig_id_readout: bool,
    /// Whether to render a UI for reading the PCB ID
    /// The PCB ID is used for logging purposes, but also and if the `BundleIdentification::PcbId` is used
    /// it is used to identify the bundle to be loaded
    pub pcb_id_readout: bool,
    /// Whether to render a UI for reading the box ID
    /// The box ID is used for logging purposes, but also and if the `BundleIdentification::BoxId` is used
    /// it is used to identify the bundle to be loaded
    pub box_id_readout: bool,
}

impl Config {
    /// Create a new configuration with default values
    /// (no port, no bundle identification method, no readouts)
    pub const fn new() -> Self {
        Self {
            port: None,
            bundle_identification: BundleIdentification::None,
            test_jig_id_readout: false,
            pcb_id_readout: false,
            box_id_readout: false,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

/// The identification method used to identify a bundle to be loaded.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Deserialize)]
pub enum BundleIdentification {
    /// No identification method is used - just load the first bundle found
    /// and - depending on the concrete `BundleLoader` implementation and its configuration -
    /// remove it from the storage.
    None,
    /// Use the PCB ID as the bundle ID
    PcbId,
    /// Use the DEVICE box ID as the bundle ID
    BoxId,
}

/// Run the factory
///
/// # Arguments
/// - `conf` - The configuration of the factory
/// - `bundle_dir` - The directory where a loaded bundle is temporarily stored for processing
/// - `loader` - The loader used to load the bundle
pub async fn run<T>(conf: &Config, bundle_dir: &Path, loader: T) -> anyhow::Result<()>
where
    T: loader::BundleLoader,
{
    let mut terminal = ratatui::init();

    let model = Arc::new(Model::new());

    let result = select(
        View::new(&model, &mut terminal).run(),
        Task::new(model.clone(), conf, bundle_dir, loader).run(&mut Input::new(&model)),
    )
    .coalesce()
    .await;

    ratatui::restore();

    result
}
