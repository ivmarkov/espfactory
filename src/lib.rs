#![allow(async_fn_in_trait)]

use std::path::Path;

use alloc::sync::Arc;

use embassy_futures::select::select;

use embassy_sync::signal::Signal;
use input::Input;
use model::Model;
use serde::{Deserialize, Serialize};
use task::Task;
use utils::futures::Coalesce;
use view::View;

pub use logger::LOGGER;

extern crate alloc;

mod bundle;
mod efuse;
mod flash;
mod input;
pub mod loader;
mod logger;
mod model;
mod task;
mod utils;
mod view;

/// The configuration of the factory
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Do not flash or eFuse, just print the commands that would be executed
    #[serde(default)]
    pub dry_run: bool,
    /// The serial port to use for communication with the device
    ///
    /// If not provided, the first available port where an ESP chip is
    /// detected will be used
    #[serde(default)]
    pub port: Option<String>,
    /// The flash speed to use for flashing the device
    ///
    /// If not provided, the default speed will be used
    #[serde(default)]
    pub flash_speed: Option<u32>,
    /// The method used to identify the bundle to be loaded
    #[serde(default)]
    pub bundle_identification: BundleIdentification,
    /// Whether to render a UI for reading the test jig ID
    ///
    /// The test jig Id is only read and used for logging purposes
    #[serde(default)]
    pub test_jig_id_readout: bool,
    /// Whether to render a UI for reading the PCB ID
    ///
    /// The PCB ID is used for logging purposes, but also and if the `BundleIdentification::PcbId` is used
    /// it is used to identify the bundle to be loaded
    #[serde(default)]
    pub pcb_id_readout: bool,
    /// Whether to render a UI for reading the box ID
    ///
    /// The box ID is used for logging purposes, but also and if the `BundleIdentification::BoxId` is used
    /// it is used to identify the bundle to be loaded
    #[serde(default)]
    pub box_id_readout: bool,
    /// Whether to skip all confirmation screens
    #[serde(default)]
    pub skip_confirmations: bool,
    /// Whether to supply the default partition table if the loaded bundle does not contain one
    #[serde(default = "default_bool::<true>")]
    pub supply_default_partition_table: bool,
    /// Whether to supply the default bootloader if the loaded bundle does not contain one
    #[serde(default = "default_bool::<true>")]
    pub supply_default_bootloader: bool,
}

impl Config {
    /// Create a new configuration with default values
    /// (no port, no bundle identification method, no readouts)
    pub const fn new() -> Self {
        Self {
            dry_run: false,
            port: None,
            flash_speed: None,
            bundle_identification: BundleIdentification::None,
            test_jig_id_readout: false,
            pcb_id_readout: false,
            box_id_readout: false,
            skip_confirmations: false,
            supply_default_partition_table: true,
            supply_default_bootloader: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

const fn default_bool<const V: bool>() -> bool {
    V
}

/// The identification method used to identify a bundle to be loaded.
#[derive(Copy, Clone, Default, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum BundleIdentification {
    /// No identification method is used - just load the first bundle found
    /// and - depending on the concrete `BundleLoader` implementation and its configuration -
    /// remove it from the storage.
    #[default]
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
pub async fn run<T>(conf: &Config, bundle_dir: &Path, bundle_loader: T) -> anyhow::Result<()>
where
    T: loader::BundleLoader,
{
    let mut terminal = ratatui::init();

    let signal = Arc::new(Signal::new());

    let model = Arc::new(Model::new(signal.clone()));

    LOGGER.swap_signal(Some(signal));
    let _guard = scopeguard::guard((), |_| {
        LOGGER.swap_signal(None);
    });

    let result = select(
        View::new(&model, &mut terminal).run(),
        Task::new(model.clone(), conf, bundle_dir, bundle_loader).run(&Input::new(&model)),
    )
    .coalesce()
    .await;

    ratatui::restore();

    result
}
