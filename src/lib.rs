#![allow(async_fn_in_trait)]

use alloc::sync::Arc;

use anyhow::Context;

use embassy_futures::select::select3;

use input::{LogInput, LogInputOutcome};
use model::Model;
use serde::{Deserialize, Serialize};
use task::Task;
use ui::input::Input;
use ui::view::View;
use utils::futures::Coalesce;

pub use logger::LOGGER;

extern crate alloc;

pub mod loader;
pub mod uploader;

mod bundle;
mod efuse;
mod flash;
mod input;
mod logger;
mod model;
mod monitor;
mod task;
mod ui;
mod utils;

/// The configuration of the factory
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Do not flash, just print the commands that would be executed
    #[serde(default)]
    pub flash_dry_run: bool,
    /// Do not eFuse, just print the commands that would be executed
    #[serde(default = "default_bool::<true>")]
    pub efuse_dry_run: bool,
    /// Whether to protect the keys to be burned in the eFuse
    #[serde(default)]
    pub efuse_protect_keys: bool,
    /// Whether to protect the digests to be burned in the eFuse
    #[serde(default)]
    pub efuse_protect_digests: bool,
    /// Whether to in-place encrypt the bootloader, partition-table
    /// and all images going to partitions marked as encrypted.
    /// Requires exactly one key with purpose `XTS_AES_128_KEY`
    /// to be present in the bundle
    #[serde(default)]
    pub flash_encrypt: bool,
    /// The serial port to use for communication with the device
    ///
    /// If not provided, the first available port where an ESP chip is
    /// detected will be used
    #[serde(default)]
    pub port: Option<String>,
    /// Do not use a stub when flashing
    #[serde(default)]
    pub flash_no_stub: bool,
    /// Erase flash prior to flashing
    /// Only works if Secure Download mode is not enabled
    #[serde(default)]
    pub flash_erase: bool,
    /// Reset empty partitions by writing 0xff to the entire partition
    /// Works also when Secure Download mode is enabled
    /// For encrypted partitions, will write pre-encrypted 0xff sequences
    /// (Else ESP-IDF might complain for reading bogus data from those)
    #[serde(default)]
    pub reset_empty_partitions: bool,
    /// Use `esptool.py` for flashing the device
    #[serde(default)]
    pub flash_esptool: bool,
    /// The flash speed to use for flashing the device
    ///
    /// If not provided, the default speed will be used
    #[serde(default)]
    pub flash_speed: Option<u32>,
    /// The eFuse speed to use for burning the device eFuse
    ///
    /// If not provided, the default speed will be used
    #[serde(default)]
    pub efuse_speed: Option<u32>,
    /// Whether to ignore failed readouts of the eFuses
    /// (eFuse reading will fail if the device has a Secure Download enabled)
    #[serde(default)]
    pub efuse_ignore_failed_readouts: bool,
    /// The type of device app run to perform
    #[serde(default)]
    pub app_run: AppRun,
    /// The method used to identify the bundle to be loaded
    #[serde(default)]
    pub bundle_identification: BundleIdentification,
    /// Instead of reading the Test JIG ID, hard-code its value here.
    ///
    /// The test JIG ID is used for logging purposes.
    ///
    /// NOTE: If `test_jig_id_readout` is `true`, this value will be ignored.
    #[serde(default)]
    pub test_jig_id: String,
    /// Whether to render a UI for reading the test JIG ID
    ///
    /// The test JIG ID is only read and used for logging purposes
    #[serde(default)]
    pub test_jig_id_readout: bool,
    /// Whether to render a UI for reading the PCB ID
    ///
    /// The PCB ID is used for logging purposes, but also and if the `BundleIdentification::PcbId` is used
    /// it is used to identify the bundle to be loaded
    #[serde(default)]
    pub pcb_id_readout: bool,
    /// Whether to render a UI for reading the Device ID
    ///
    /// The Device ID is used for logging purposes, but also and if the `BundleIdentification::DeviceId` is used
    /// it is used to identify the bundle to be loaded
    #[serde(default)]
    pub device_id_readout: bool,
    /// Whether to skip all confirmation screens
    #[serde(default)]
    pub skip_confirmations: bool,
    /// Whether to supply the default partition table if the loaded bundle does not contain one
    #[serde(default = "default_bool::<true>")]
    pub supply_default_partition_table: bool,
    /// Whether to supply the default bootloader if the loaded bundle does not contain one
    #[serde(default = "default_bool::<true>")]
    pub supply_default_bootloader: bool,
    /// When a base bundle is used: whether to overwrite the base bundle images with the non-base ones
    /// during the bundles' merge operation
    #[serde(default)]
    pub overwrite_on_merge: bool,
    /// Whether to print stack backtraces in the error messages and in the logs
    #[serde(default)]
    pub print_backtraces: bool,
    /// Whether to run the app without the interactive console UI
    #[serde(default)]
    no_ui: bool,
    /// Only relevant with the interactive console UI:
    /// The length of the log buffer
    #[serde(default = "default_usize::<1000>")]
    log_buffer_len: usize,
}

impl Config {
    /// Create a new configuration with default values
    pub const fn new() -> Self {
        Self {
            flash_dry_run: false,
            efuse_dry_run: true,
            efuse_ignore_failed_readouts: false,
            efuse_protect_keys: false,
            efuse_protect_digests: false,
            port: None,
            flash_no_stub: false,
            flash_erase: false,
            reset_empty_partitions: false,
            flash_esptool: false,
            flash_encrypt: false,
            flash_speed: None,
            efuse_speed: None,
            app_run: AppRun::Disabled,
            bundle_identification: BundleIdentification::None,
            test_jig_id: String::new(),
            test_jig_id_readout: false,
            pcb_id_readout: false,
            device_id_readout: false,
            skip_confirmations: false,
            supply_default_partition_table: true,
            supply_default_bootloader: true,
            overwrite_on_merge: false,
            print_backtraces: false,
            no_ui: false,
            log_buffer_len: 1000,
        }
    }

    /// Change the configuration so that it does the right thing
    /// if the chip was already provisioned
    pub fn reprovision(&mut self) {
        // eFUSEs were most likely burned already, set dry run mode
        self.efuse_dry_run = true;
        // Erase flash so that OTA partitions, phy-init, coredump and so on are reset
        self.flash_erase = true;
    }

    /// Change the configuration so that it does the right thing
    /// if the chip already has Secure Download mode enabled
    pub fn secure_download(&mut self) {
        // eFUSEs were most likely burned already, set dry run mode
        self.efuse_dry_run = true;
        // Flash erasing does not work in Secure Download mode, disable
        self.flash_erase = false;
        // Flash stub does not work in Secure Download mode, disable
        self.flash_no_stub = true;
        // `espflash` does not work in Secure Download mode, disable
        self.flash_esptool = true;
        // Flash erase does not work in Secure Download mode, use reset empty partitions instead
        self.reset_empty_partitions = true;
        // Can't really read from eFuse when Secure Download mode is enabled
        self.efuse_ignore_failed_readouts = true;
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

const fn default_usize<const V: usize>() -> usize {
    V
}

/// Parameters for extracing the bundle ID from the Device ID or the PCB ID
#[derive(Clone, Default, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct BundleIdentificationParsing {
    /// An optional regular expression to grep only a fraction of the Device ID
    pub regex: Option<String>,
    /// Trim the Device ID before using it as the bundle ID
    #[serde(default)]
    pub trim: bool,
    /// Assume the Device ID is a number and convert it to a numeric value
    #[serde(default)]
    pub numeric: bool,
}

impl BundleIdentificationParsing {
    /// Parse the source string and extract the bundle ID
    pub fn parse(&self, source: &str) -> anyhow::Result<String> {
        let mut id = source.to_string();

        if let Some(regex) = &self.regex {
            let re = regex::Regex::new(regex).expect("Invalid regex");
            if let Some(captures) = re.captures(&id) {
                id = captures
                    .get(1)
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default();
            }
        }

        if self.trim {
            id = id.trim().to_string();
        }

        if id.is_empty() {
            Err(anyhow::anyhow!(
                "Parsing bundle identification yields an empty string"
            ))?;
        }

        if self.numeric {
            id = id
                .trim()
                .parse::<u64>()
                .map(|n| n.to_string())
                .with_context(|| {
                    format!("Parsing bundle identification `{}` as a number failed", id)
                })?;
        }

        Ok(id)
    }
}

/// The identification method used to identify a bundle to be loaded.
#[derive(Clone, Default, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(tag = "field")]
pub enum BundleIdentification {
    /// No identification method is used - just load the first bundle found
    /// and - depending on the concrete `BundleLoader` implementation and its configuration -
    /// remove it from the storage.
    #[default]
    None,
    /// Extract the bundle ID from the Device ID
    DeviceId(BundleIdentificationParsing),
    /// Extract the bundle ID from the PCB ID
    PcbId(BundleIdentificationParsing),
}

/// The type of device app run to perform
#[derive(Clone, Default, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AppRun {
    /// Do not run the app
    #[default]
    Disabled,
    /// Run the app for a given number of seconds
    ForSecs { secs: u32 },
    /// Run the app until a given pattern is matched in the app logs
    /// or until a timeout is reached which would signify an unsuccessful app run
    MatchPattern { pattern: String, timeout_secs: u32 },
}

/// Run the factory
///
/// # Arguments
/// - `conf` - The configuration of the factory
/// - `log_level` - The log level to use
/// - `bundle_dir` - The directory where a loaded bundle is temporarily stored for processing
/// - `bundle_base_loader` - An optional loader used to load the base bundle; the base bundle (if used)
///   usually contains the device-independent payloads like the bootloader, the partition image
///   and the factory app image
/// - `bundle_loader` - The loader used to load the bundle; in case `bundle_base_loader` is used, this
///   loader is used to load the device-specific payloads like the NVS partitions. The two bundles are then merged
/// - `bundle_logs_uploader` - The uploader used to upload the logs from the device provisioning to the server
pub async fn run<B, L, U>(
    conf: &Config,
    log_level: log::LevelFilter,
    bundle_base_loader: Option<B>,
    bundle_loader: L,
    bundle_logs_uploader: U,
) -> anyhow::Result<()>
where
    B: loader::BundleLoader,
    L: loader::BundleLoader,
    U: uploader::BundleLogsUploader,
{
    let mut terminal = (!conf.no_ui).then(ratatui::init);
    let area = terminal
        .as_mut()
        .map(|terminal| terminal.get_frame().area());

    let model = Arc::new(Model::new(
        log_level,
        conf.no_ui,
        if conf.no_ui {
            0
        } else {
            conf.log_buffer_len.min(5000)
        },
        area.map(|area| area.width).unwrap_or(0),
        area.map(|area| area.height).unwrap_or(0),
    ));

    LOGGER.swap_model(Some(model.clone()));
    let _guard = scopeguard::guard((), |_| {
        LOGGER.swap_model(None);
    });

    let result = if let Some(mut terminal) = terminal {
        let input = Input::new(&model);

        select3(
            View::new(&model, &mut terminal).run(),
            Task::new(
                model.clone(),
                conf,
                bundle_base_loader,
                bundle_loader,
                bundle_logs_uploader,
            )
            .run(&input),
            run_log(&model, &input),
        )
        .coalesce()
        .await
    } else {
        Task::new(
            model.clone(),
            conf,
            bundle_base_loader,
            bundle_loader,
            bundle_logs_uploader,
        )
        .run(input::Stdin)
        .await
    };

    if !conf.no_ui {
        ratatui::restore();
    }

    result
}

/// Run the interaction with the logs view
async fn run_log(model: &Model, mut input: impl LogInput) -> anyhow::Result<()> {
    loop {
        let action = input.get().await;

        model.access_mut(|inner| {
            let log = &mut inner.logs.buffered;

            match action {
                LogInputOutcome::Home => log.home_end_x(true),
                LogInputOutcome::End => log.home_end_x(false),
                LogInputOutcome::Left => log.scroll_x(true),
                LogInputOutcome::Right => log.scroll_x(false),
                LogInputOutcome::LogHome => log.home_end_y(true),
                LogInputOutcome::LogEnd => log.home_end_y(false),
                LogInputOutcome::PgUp => log.page_scroll_y(true),
                LogInputOutcome::PgDown => log.page_scroll_y(false),
                LogInputOutcome::Up => log.scroll_y(true),
                LogInputOutcome::Down => log.scroll_y(false),
            }

            ((), true)
        });
    }
}
