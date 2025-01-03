use core::fmt::{self, Display};
use core::future::Future;

use std::fmt::Write as _;
use std::fs::{self, DirEntry, File};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use alloc::sync::Arc;

use anyhow::Context;

use crossterm::event::KeyCode;

use embassy_futures::select::{select, select3, Either, Either3};
use embassy_time::{Duration, Ticker};

use espflash::flasher::ProgressCallbacks;

use log::{error, info};

use crate::bundle::{Bundle, Efuse, Params, ProvisioningStatus};
use crate::efuse;
use crate::flash;
use crate::input::{ConfirmOutcome, Input};
use crate::loader::BundleLoader;
use crate::model::{Model, Processing, Provision, Readout, State};
use crate::uploader::BundleLogsUploader;
use crate::utils::futures::unblock;
use crate::{BundleIdentification, Config, LOGGER};

extern crate alloc;

/// A task that runs the factory application and represents the lifecycle states of provisioning a bundle
/// (readouts, preparing, provisioning, etc.)
pub struct Task<'a, B, L, U> {
    model: Arc<Model>,
    conf: &'a Config,
    bundle_dir: &'a Path,
    bundle_base_loader: Option<B>,
    bundle_loader: L,
    bundle_logs_uploader: U,
}

impl<'a, B, L, U> Task<'a, B, L, U>
where
    B: BundleLoader,
    L: BundleLoader,
    U: BundleLogsUploader,
{
    const BUNDLE_TEMP_DIR_NAME: &'static str = "temp";
    const BUNDLE_LOADED_DIR_NAME: &'static str = "loaded";

    /// Create a new task
    ///
    /// Arguments:
    /// - `model` - the model (states) of the application
    ///   Shared between the task, the UI (`View`) and the input processing (`Input`), i.e.
    ///   the task modifies the model, the UI renders the model and the input processing triggers model changes on terminal resize events (MVC)
    /// - `conf` - the configuration of the task
    /// - `bundle_dir` - the directory where the bundles are stored
    /// - `bundle_base_loader` - An optional loader used to load the base bundle; the base bundle (if used)
    ///   usually contains the device-independent payloads like the bootloader, the partition image
    ///   and the factory app image
    /// - `bundle_loader` - The loader used to load the bundle; in case `bundle_base_loader` is used, this
    ///   loader is used to load the device-specific payloads like the NVS partitions. The two bundles are then merged
    /// - `bundle_logs_uploader` - The uploader used to upload the logs from the device provisioning to the server
    pub fn new(
        model: Arc<Model>,
        conf: &'a Config,
        bundle_dir: &'a Path,
        bundle_base_loader: Option<B>,
        bundle_loader: L,
        bundle_logs_uploader: U,
    ) -> Self {
        Self {
            model,
            conf,
            bundle_dir,
            bundle_base_loader,
            bundle_loader,
            bundle_logs_uploader,
        }
    }

    /// Run the factory bundle provisioning task in a loop as follows:
    /// - Step 1: eFuse readouts (read the necessary IDs from the chip eFuse memory)
    /// - Step 2: Readouts (read the necessary IDs from the user, e.g. test jig ID, PCB ID, box ID)
    /// - Step 3: Load and prepare the (next) bundle to be provisioned, possibly using one of the readouts as a bundle ID
    ///   by fetching the bundle content using the bundle loader, and then creating a `Bundle` instance
    /// - Step 4: Provision the bundle by flashing and optionally efusing the chip with the bundle content
    /// - Step 5: Save the log output to a file and upload it to the server
    ///
    /// Repeat the above steps until the user quits
    ///
    /// Arguments:
    /// - `input` - the input helper to process terminal events
    ///   Necessary as some states require direct user input (e.g. readouts)
    pub async fn run(&mut self, input: &Input<'_>) -> anyhow::Result<()> {
        let result = self.step(input).await;

        match result {
            Err(TaskError::Quit) => {
                info!("Quit by user request");
                Ok(())
            }
            Err(TaskError::Other(err)) => Err(err)?,
            Ok(_) | Err(TaskError::Canceled) | Err(TaskError::Retry) => {
                unreachable!("Task canceled or retry by user request");
            }
        }
    }

    async fn step(&mut self, input: &Input<'_>) -> Result<(), TaskError> {
        loop {
            {
                let log_file = super::logger::file::start()?;
                LOGGER.lock(|logger| logger.swap_out(Some(log_file)));
            }

            let _guard = scopeguard::guard((), |_| {
                LOGGER.lock(|logger| logger.swap_out(None));
            });

            let (bundle_id, bundle_name, summary) = 'steps: loop {
                let mut summary = Vec::new();

                let bundle_id = loop {
                    summary.clear();

                    let result = Self::handle(
                        &self.model.clone(),
                        self.step1_prepare_efuse_readout(input),
                        "Preparing eFuse readouts failed",
                        input,
                    )
                    .await;

                    match result {
                        Ok(_) => (),
                        Err(TaskError::Canceled) | Err(TaskError::Retry) => continue,
                        Err(other) => Err(other)?,
                    }

                    let result = self.step2_readout(input).await;

                    match result {
                        Ok(_) => (),
                        Err(TaskError::Canceled) => continue,
                        Err(TaskError::Retry) => unreachable!(),
                        Err(other) => Err(other)?,
                    }

                    self.model.access(|state| {
                        let readout = state.readout();

                        for (name, value) in &readout.efuse_readouts {
                            summary.push((name.clone(), value.clone()));
                        }

                        for (name, value) in &readout.readouts {
                            summary.push((name.clone(), value.clone()));
                        }
                    });

                    break loop {
                        // TODO: Not very efficient
                        let readout = self.model.access(|state| state.readout().clone());

                        let result = Self::handle(
                            &self.model.clone(),
                            self.step3_prepare(input),
                            "Preparing a bundle failed",
                            input,
                        )
                        .await;

                        match result {
                            Ok(bundle_id) => break bundle_id,
                            Err(TaskError::Canceled) => continue 'steps,
                            Err(TaskError::Retry) => {
                                self.model.modify(|state| *state = State::Readout(readout));

                                continue;
                            }
                            Err(other) => Err(other)?,
                        };
                    };
                };

                break loop {
                    if !self.conf.skip_confirmations {
                        match input.wait_confirm().await.into() {
                            Ok(_) => (),
                            Err(TaskError::Canceled) => continue 'steps,
                            Err(TaskError::Retry) => unreachable!(),
                            Err(other) => Err(other)?,
                        }
                    }

                    // TODO: Not very efficient
                    let provision = self.model.access(|state| state.provision().clone());

                    let result = Self::handle(
                        &self.model.clone(),
                        self.step4_provision(input),
                        &format!("Provisioning bundle `{}` failed", provision.bundle.name),
                        input,
                    )
                    .await;

                    match result {
                        Ok(_) => break (bundle_id, provision.bundle.name.clone(), summary),
                        Err(TaskError::Canceled) => continue 'steps,
                        Err(TaskError::Retry) => {
                            self.model
                                .modify(|state| *state = State::Provision(provision));

                            continue;
                        }
                        Err(other) => Err(other)?,
                    }
                };
            };

            // Step 5
            if let Some(log_file) = LOGGER.lock(|logger| logger.swap_out(None)) {
                let log = super::logger::file::finish(log_file, &summary)?;
                self.bundle_logs_uploader
                    .upload_logs(log, bundle_id.as_deref(), &bundle_name)
                    .await?;
            }

            if !self.conf.skip_confirmations
                && matches!(input.wait_confirm().await, ConfirmOutcome::Quit)
            {
                break;
            }
        }

        Ok(())
    }

    /// Step 1:
    /// Prepare the eFuse readouts by reading those from the chip eFuse memory
    ///
    /// Displays a progress info while reading the eFuse values
    async fn step1_prepare_efuse_readout(
        &mut self,
        input: &Input<'_>,
    ) -> anyhow::Result<(), TaskError> {
        self.model
            .modify(|state| *state = State::Processing(Processing::new(" Read eFuse IDs ")));

        Self::process(&self.model.clone(), self.prep_efuse_readouts(), input).await
    }

    /// Step 2:
    /// Process the readouts state by visualizing the eFuse readouts (if any) and
    /// reading the necessary IDs from the user (if any)
    async fn step2_readout(&mut self, input: &Input<'_>) -> Result<(), TaskError> {
        let init = |readouts: &mut Readout| {
            readouts.readouts.clear();
            readouts.active = 0;

            if self.conf.test_jig_id_readout {
                readouts
                    .readouts
                    .push(("Test JIG ID".to_string(), "".to_string()));
            }

            if self.conf.pcb_id_readout {
                readouts
                    .readouts
                    .push(("PCB ID".to_string(), "".to_string()));
            }

            if self.conf.box_id_readout {
                readouts
                    .readouts
                    .push(("Box ID".to_string(), "".to_string()));
            }
        };

        self.model.modify(|state| {
            let readouts = state.readout_mut();
            init(readouts);
        });

        let mut result = Ok(());

        while result.is_ok() && !self.model.access(|state| state.readout().is_ready()) {
            let key = input.get().await;

            self.model.access_mut(|state| {
                let readouts = state.readout_mut();

                let readout = &mut readouts.readouts[readouts.active];

                match Input::key_m(&key) {
                    Input::NEXT => {
                        if !readout.1.is_empty() {
                            readouts.active += 1;
                            info!("Readout `{}`: `{}`", readout.0, readout.1);
                            return true;
                        }
                    }
                    Input::PREV => {
                        if readouts.active == 0 && readout.1.is_empty() {
                            result = Err(TaskError::Canceled);
                            return false;
                        }

                        init(readouts);
                        info!("Readouts reset");
                        return true;
                    }
                    Input::QUIT => {
                        result = Err(TaskError::Quit);
                        return false;
                    }
                    (modifiers, code) => {
                        if modifiers.is_empty() {
                            match code {
                                KeyCode::Backspace => {
                                    readout.1.pop();
                                    return true;
                                }
                                KeyCode::Char(ch) => {
                                    readout.1.push(ch);
                                    return true;
                                }
                                _ => (),
                            }
                        }
                    }
                }

                false
            });
        }

        result
    }

    /// Step 3:
    /// Prepare the bundle to be provisioned by loading it from the storage
    async fn step3_prepare(
        &mut self,
        input: &Input<'_>,
    ) -> anyhow::Result<Option<String>, TaskError> {
        let (_test_jig_id, pcb_id, box_id) = self.model.access(|state| {
            let readouts = state.readout();
            let mut offset = 0;

            let test_jig_id = if self.conf.test_jig_id_readout {
                let readout = readouts.readouts[offset].1.clone();

                offset += 1;

                Some(readout)
            } else {
                None
            };

            let pcb_id = if self.conf.pcb_id_readout {
                let readout = readouts.readouts[offset].1.clone();

                offset += 1;

                Some(readout)
            } else {
                None
            };

            let box_id = if self.conf.box_id_readout {
                let readout = readouts.readouts[offset].1.clone();
                Some(readout)
            } else {
                None
            };

            (test_jig_id, pcb_id, box_id)
        });

        self.model
            .modify(|state| *state = State::Processing(Processing::new(" Preparing bundle ")));

        let bundle_id = match self.conf.bundle_identification {
            BundleIdentification::None => None,
            BundleIdentification::PcbId => pcb_id,
            BundleIdentification::BoxId => box_id,
        };

        Self::process(
            &self.model.clone(),
            self.prep_bundle(bundle_id.as_deref()),
            input,
        )
        .await?;

        Ok(bundle_id)
    }

    /// Step 4:
    /// Provision the bundle by flashing and optionally efusing the chip with the bundle content
    async fn step4_provision(&mut self, input: &Input<'_>) -> anyhow::Result<(), TaskError> {
        match select(self.prov_bundle(), input.swallow()).await {
            Either::First(result) => result.map_err(TaskError::Other),
        }
    }

    //
    // Helper methods
    //

    /// Prepare the eFuse readouts by reading those from the chip eFuse memory
    async fn prep_efuse_readouts(&mut self) -> anyhow::Result<()> {
        static EFUSE_VALUES: &[&str] = &[
            "MAC",
            "WAFER_VERSION_MAJOR",
            "WAFER_VERSION_MINOR",
            // Not available on all chips
            "OPTIONAL_UNIQUE_ID",
            "FLASH_TYPE",
            "FLASH_VENDOR",
            "FLASH_CAP",
            "PSRAM_CAP",
            "PSRAM_TYPE",
            "PSRAMP_VENDOR",
        ];

        self.model.modify(|state| {
            state.processing_mut().status = "Reading Chip IDs from eFuse".to_string();
        });

        info!("About to read Chip IDs from eFuse");

        let efuse_chip: Option<String> = None; // TODO
        let efuse_port = self.conf.port.clone();
        let efuse_baud = self.conf.efuse_speed.map(|speed| speed.to_string());

        let efuse_values = unblock("efuse-summary", move || {
            let efuse_values = efuse::summary(
                efuse_chip.as_deref(),
                efuse_port.as_deref(),
                efuse_baud.as_deref(),
                EFUSE_VALUES.iter().copied(),
            )?;

            let efuse_values = efuse_values
                .iter()
                .filter_map(|(k, v)| {
                    v.value.as_str().and_then(|v| {
                        EFUSE_VALUES
                            .iter()
                            .find(|&x| x == k)
                            .map(|&x| (x.to_string(), v.to_string()))
                    })
                })
                .collect::<Vec<_>>();

            Ok(efuse_values)
        })
        .await?;

        for (key, value) in efuse_values.iter() {
            info!("Chip {key}: {value}");
        }

        self.model
            .modify(move |state| *state = State::Readout(Readout::new_with_efuse(efuse_values)));

        Ok(())
    }

    /// Prepare the bundle to be provisioned by creating a `Bundle` instance from the loaded bundle content
    /// in the bundle workspace directory
    async fn prep_bundle(&mut self, bundle_id: Option<&str>) -> anyhow::Result<()> {
        let bundle = Self::prep_one_bundle(
            &self.model,
            self.bundle_dir,
            bundle_id,
            &mut self.bundle_loader,
            self.bundle_base_loader.is_none() && self.conf.supply_default_partition_table,
            self.bundle_base_loader.is_none() && self.conf.supply_default_bootloader,
        )
        .await?;

        let bundle = if let Some(base_loader) = self.bundle_base_loader.as_mut() {
            let mut base_bundle = Self::prep_one_bundle(
                &self.model,
                self.bundle_dir,
                None,
                base_loader,
                self.conf.supply_default_partition_table,
                self.conf.supply_default_bootloader,
            )
            .await?;

            self.model.modify(|state| {
                state.processing_mut().status =
                    format!("Merging {} and {}", base_bundle.name, bundle.name);
            });

            base_bundle.add(bundle, self.conf.overwrite_on_merge)?;

            base_bundle
        } else {
            bundle
        };

        self.model.modify(move |state| {
            *state = State::Provision(Provision {
                bundle,
                provisioning: false,
            })
        });

        Ok(())
    }

    /// Provision the bundle by flashing and optionally efusing the chip with the bundle content
    async fn prov_bundle(&mut self) -> anyhow::Result<()> {
        self.model.modify(|state| {
            let ps = state.provision_mut();
            ps.provisioning = true;

            info!("About to provision bundle `{}`", ps.bundle.name);

            ps.bundle.set_status_all(ProvisioningStatus::Pending);
        });

        let (chip, flash_size, flash_data) = self.model.access(|state| {
            let ps = state.provision();

            (
                ps.bundle.params.chip,
                ps.bundle.params.flash_size,
                ps.bundle.get_flash_data().collect::<Vec<_>>(),
            )
        });

        info!(
            "About to flash data: Chip={chip:?}, Flash Size={flash_size:?}, Images N={}",
            flash_data.len()
        );

        let flash_port = self.conf.port.clone();
        let flash_speed = self.conf.flash_speed;
        let flash_model = self.model.clone();
        let flash_dry_run = self.conf.flash_dry_run;

        unblock("flash", move || {
            flash::flash(
                flash_port.as_deref(),
                chip,
                flash_speed,
                flash_size,
                flash_data,
                flash_dry_run,
                FlashProgress::new(flash_model),
            )
        })
        .await?;

        info!("Flash complete");

        info!("About to burn eFuses");

        let model = self.model.clone();

        let efuse_chip: Option<String> = None; // TODO
        let efuse_port = self.conf.port.clone();
        let efuse_baud = self.conf.efuse_speed.map(|speed| speed.to_string());
        let efuse_dry_run = self.conf.efuse_dry_run;

        unblock("efuse-burn", move || {
            Self::burn(
                &model,
                efuse_chip.as_deref(),
                efuse_port.as_deref(),
                efuse_baud.as_deref(),
                efuse_dry_run,
            )
        })
        .await?;

        info!("Burn complete");

        let bundle_loaded_dir = self.bundle_dir.join(Self::BUNDLE_LOADED_DIR_NAME);
        fs::create_dir_all(&bundle_loaded_dir)
            .context("Creating loaded bundle directory failed")?;

        for entry in bundle_loaded_dir.read_dir()? {
            let entry = entry?;
            fs::remove_file(entry.path()).context("Emptying loaded bundle directory failed")?;
        }

        self.model.modify(|state| {
            info!(
                "Provisioning bundle `{}` complete",
                state.provision().bundle.name
            );

            state.success(
                format!(" {} ", state.provision().bundle.name),
                "Provisioning complete.",
            );
        });

        Ok(())
    }

    /// Load a bundle from the storage of the bundle loader into the bundle workspace directory
    async fn load_one_bundle<T>(
        model: &Model,
        bundle_dir: &Path,
        bundle_id: Option<&str>,
        mut loader: T,
    ) -> anyhow::Result<PathBuf>
    where
        T: BundleLoader,
    {
        let bundle = loop {
            model.modify(|state| {
                state.processing_mut().status = "Checking".into();
            });

            if bundle_id.is_none() {
                // - Only preserve the loaded bundle if no bundle ID is provided
                //   (i.e. a random bundle is to be downloaded, used, and possibly deleted from the server)
                // - For bundles with a bundle ID, always re-download the bundle using the loader

                let loaded_path = bundle_dir.join(Self::BUNDLE_LOADED_DIR_NAME);
                fs::create_dir_all(&loaded_path)
                    .context("Creating loaded bundle directory failed")?;

                let files: Result<Vec<DirEntry>, _> = fs::read_dir(&loaded_path)
                    .context("Listing loaded bundle directory failed")?
                    .collect();

                let files = files.context("Listing loaded bundle directory failed")?;

                if files.len() > 1 {
                    anyhow::bail!("More than one bundle found in the bundle workspace directory");
                }

                if files.len() == 1 {
                    break files[0].path();
                }
            }

            model.modify(|state| {
                state.processing_mut().status = "Fetching".into();
            });

            let mut bundle_temp_path = bundle_dir.join(Self::BUNDLE_TEMP_DIR_NAME).join("bundle");
            fs::create_dir_all(bundle_temp_path.parent().unwrap())
                .context("Creating the temp bundle directory failed")?;

            let bundle_name = {
                let result = {
                    let mut temp_file = File::create(&bundle_temp_path)
                        .context("Creating temp bundle file failed")?;

                    loader.load(&mut temp_file, bundle_id).await
                };

                match result {
                    Ok(bundle_name) => {
                        let bundle_new_temp_path = bundle_dir.join(&bundle_name);
                        fs::rename(&bundle_temp_path, &bundle_new_temp_path)
                            .context("Renaming temp bundle file failed")?;

                        bundle_temp_path = bundle_new_temp_path;

                        bundle_name
                    }
                    Err(err) => Err(err)?,
                }
            };

            if bundle_id.is_none() {
                // Move the loaded random bundle to the loaded path so as not to lose it if the user interrupts the provisioning process

                let bundle_loaded_path = bundle_dir
                    .join(Self::BUNDLE_LOADED_DIR_NAME)
                    .join(&bundle_name);
                fs::create_dir_all(bundle_loaded_path.parent().unwrap())
                    .context("Creating the loaded bundle directory failed")?;

                fs::rename(bundle_temp_path, bundle_loaded_path)
                    .context("Moving the temp bundle into the loaded bundle directory failed")?;
            } else {
                break bundle_temp_path;
            }
        };

        info!("Bundle loaded into file `{}`", bundle.display());

        Ok(bundle)
    }

    /// Prepare a bundle to be provisioned by creating a `Bundle` instance from the loaded bundle content
    /// in the bundle workspace directory
    async fn prep_one_bundle<T>(
        model: &Model,
        bundle_dir: &Path,
        bundle_id: Option<&str>,
        loader: T,
        supply_default_partition_table: bool,
        supply_default_bootloader: bool,
    ) -> anyhow::Result<Bundle>
    where
        T: BundleLoader,
    {
        let bundle_path = Self::load_one_bundle(model, bundle_dir, bundle_id, loader).await?;

        let bundle_name = bundle_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string(); // TODO

        model.modify(|state| {
            state.processing_mut().status = format!("Processing {bundle_name}");
        });

        info!("About to prep bundle file `{}`", bundle_path.display());

        let mut bundle_file =
            File::open(bundle_path).context("Opening the loaded bundle file failed")?;

        Bundle::create(
            bundle_name,
            Params::default(),
            &mut bundle_file,
            supply_default_partition_table,
            supply_default_bootloader,
        )
    }

    fn burn(
        model: &Model,
        chip: Option<&str>,
        port: Option<&str>,
        baud: Option<&str>,
        dry_run: bool,
    ) -> anyhow::Result<String> {
        let mut output = String::new();

        model.modify(|state| {
            let efuses = &mut state.provision_mut().bundle.efuse_mapping;

            for efuse in efuses {
                efuse.status = ProvisioningStatus::Pending;
            }
        });

        // Step 1: Burn key digests first

        let mut digests = Vec::new();

        model.access_mut(|state| {
            let efuses = &mut state.provision_mut().bundle.efuse_mapping;

            let mut changed = false;

            for efuse in efuses {
                if let Efuse::KeyDigest {
                    block,
                    digest_value,
                    purpose,
                } = &efuse.efuse
                {
                    digests.push((block.clone(), digest_value.clone(), purpose.clone()));
                }

                efuse.status = ProvisioningStatus::Pending;
                changed = true;
            }

            changed
        });

        if !digests.is_empty() {
            let digests_output = efuse::burn_key_digests(
                chip,
                port,
                baud,
                dry_run,
                digests.iter().map(|(block, digest, purpose)| {
                    (block.as_str(), digest.as_slice(), purpose.as_str())
                }),
            )
            .context("Burning key digests failed")?;

            model.modify(|state| {
                let efuses = &mut state.provision_mut().bundle.efuse_mapping;

                for efuse in efuses {
                    if let Efuse::KeyDigest { .. } = &efuse.efuse {
                        efuse.status = ProvisioningStatus::Done;
                    }
                }
            });

            write!(&mut output, "{digests_output}\n\n")?;
        }

        // Step 2: Burn keys next

        let mut keys = Vec::new();

        model.access_mut(|state| {
            let efuses = &mut state.provision_mut().bundle.efuse_mapping;

            let mut changed = false;

            for efuse in efuses {
                if let Efuse::Key {
                    block,
                    key_value,
                    purpose,
                } = &efuse.efuse
                {
                    keys.push((block.clone(), key_value.clone(), purpose.clone()));
                }

                efuse.status = ProvisioningStatus::Pending;
                changed = true;
            }

            changed
        });

        if !keys.is_empty() {
            let keys_output = efuse::burn_keys(
                chip,
                port,
                baud,
                dry_run,
                keys.iter().map(|(block, key, purpose)| {
                    (block.as_str(), key.as_slice(), purpose.as_str())
                }),
            )
            .context("Burning keys failed")?;

            model.modify(|state| {
                let efuses = &mut state.provision_mut().bundle.efuse_mapping;

                for efuse in efuses {
                    if let Efuse::Key { .. } = &efuse.efuse {
                        efuse.status = ProvisioningStatus::Done;
                    }
                }
            });

            write!(&mut output, "{keys_output}\n\n")?;
        }

        // Step 3: Finally, burn all params

        let mut params = Vec::new();

        model.access_mut(|state| {
            let efuses = &mut state.provision_mut().bundle.efuse_mapping;

            let mut changed = false;

            for efuse in efuses {
                if let Efuse::Param { name, value } = &efuse.efuse {
                    params.push((name.clone(), *value));
                }

                efuse.status = ProvisioningStatus::Pending;
                changed = true;
            }

            changed
        });

        if !params.is_empty() {
            let params_output = efuse::burn_efuses(
                chip,
                port,
                baud,
                dry_run,
                params.iter().map(|(name, value)| (name.as_str(), *value)),
            )
            .context("Burning params failed")?;

            model.modify(|state| {
                let efuses = &mut state.provision_mut().bundle.efuse_mapping;

                for efuse in efuses {
                    if let Efuse::Param { .. } = &efuse.efuse {
                        efuse.status = ProvisioningStatus::Done;
                    }
                }
            });

            write!(&mut output, "{params_output}\n\n")?;
        }

        Ok(output)
    }

    /// Handle a future failure by displaying an error message and waiting for a confirmation
    async fn handle<F, R>(
        model: &Model,
        fut: F,
        err_msg: &str,
        input: &Input<'_>,
    ) -> anyhow::Result<R, TaskError>
    where
        F: Future<Output = anyhow::Result<R, TaskError>>,
    {
        let result = fut.await;

        if let Err(TaskError::Other(err)) = result {
            error!("{err_msg}: {err:?}");

            model
                .modify(|state| state.error(format!(" {err_msg} "), format!("{err_msg}: {err:?}")));

            match input.wait_confirm().await.into() {
                Ok(_) => Err(TaskError::Retry),
                Err(err) => Err(err),
            }
        } else {
            result
        }
    }

    /// Process a future by incrementing a counter every 100 ms while the future is running
    async fn process<F, R>(model: &Model, fut: F, input: &Input<'_>) -> anyhow::Result<R, TaskError>
    where
        F: Future<Output = anyhow::Result<R>>,
    {
        let result = select3(
            Self::tick(Duration::from_millis(100), || {
                model.modify(|state| {
                    if let State::Processing(Processing { counter, .. }) = state {
                        *counter += 1;
                    }
                })
            }),
            input.wait_cancel(),
            fut,
        )
        .await;

        let result = match result {
            Either3::Second(outcome) => {
                let Err(err) = outcome.into() else {
                    unreachable!()
                };

                return Err(err);
            }
            Either3::Third(result) => result?,
        };

        Ok(result)
    }

    /// A helper to increment a counter every 100 ms
    async fn tick<F>(duration: Duration, mut f: F) -> !
    where
        F: FnMut(),
    {
        let mut tick = Ticker::every(duration);

        loop {
            tick.next().await;
            f();
        }
    }
}

/// A progress callback for flashing the bundle
struct FlashProgress {
    model: Arc<Model>,
    image: Mutex<Option<(u32, usize)>>,
}

impl FlashProgress {
    fn new(model: Arc<Model>) -> Self {
        Self {
            model,
            image: Mutex::new(None),
        }
    }
}

impl ProgressCallbacks for FlashProgress {
    fn init(&mut self, addr: u32, total: usize) {
        *self.image.lock().unwrap() = Some((addr, total));

        self.model.access_mut(|state| {
            state
                .provision_mut()
                .bundle
                .set_status(addr, ProvisioningStatus::InProgress(0))
        });
    }

    fn update(&mut self, current: usize) {
        if let Some((addr, total)) = *self.image.lock().unwrap() {
            self.model.access_mut(|state| {
                state.provision_mut().bundle.set_status(
                    addr,
                    ProvisioningStatus::InProgress((current * 100 / total) as u8),
                )
            });
        }
    }

    fn finish(&mut self) {
        if let Some((addr, _)) = self.image.lock().unwrap().take() {
            self.model.access_mut(|state| {
                state
                    .provision_mut()
                    .bundle
                    .set_status(addr, ProvisioningStatus::Done)
            });
        }
    }
}

/// A task step error
#[derive(Debug)]
enum TaskError {
    /// Retry the step by user request
    Retry,
    /// Go to the previous step by user request
    Canceled,
    /// Quit the app by user request
    Quit,
    /// Other error - display the error message or quit the app depending on the context
    Other(anyhow::Error),
}

impl From<anyhow::Error> for TaskError {
    fn from(err: anyhow::Error) -> Self {
        TaskError::Other(err)
    }
}

impl From<ConfirmOutcome> for Result<(), TaskError> {
    fn from(outcome: ConfirmOutcome) -> Self {
        match outcome {
            ConfirmOutcome::Confirmed => Ok(()),
            ConfirmOutcome::Canceled => Err(TaskError::Canceled),
            ConfirmOutcome::Quit => Err(TaskError::Quit),
        }
    }
}

impl Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskError::Retry => write!(f, "Retry"),
            TaskError::Canceled => write!(f, "Canceled"),
            TaskError::Quit => write!(f, "Quit"),
            TaskError::Other(err) => write!(f, "Error: {err:#}"),
        }
    }
}

impl std::error::Error for TaskError {}
