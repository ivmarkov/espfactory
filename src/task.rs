use core::fmt::{self, Display};
use core::future::Future;
use core::pin::pin;

use std::fmt::Write as _;
use std::io::{Seek, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use alloc::sync::Arc;

use anyhow::Context;

use embassy_futures::select::{select, select3, Either, Either3};
use embassy_time::{Duration, Ticker};

use espflash::cli::monitor::LogFormat;
use espflash::flasher::ProgressCallbacks;

use log::{error, info, warn};

use tempfile::NamedTempFile;

use crate::bundle::{Bundle, Chip, Efuse, Params, ProvisioningStatus};
use crate::flash::{self, encrypt, DEFAULT_BAUD_RATE};
use crate::input::{TaskConfirmationOutcome, TaskInput, TaskInputOutcome};
use crate::loader::BundleLoader;
use crate::model::{AppLogs, FileLogs, Model, Processing, Provision, Readout, State};
use crate::uploader::BundleLogsUploader;
use crate::utils::futures::unblock;
use crate::utils::linewrite::LineWrite;
use crate::{efuse, monitor, AppRun};
use crate::{BundleIdentification, Config};

extern crate alloc;

/// A task that runs the factory application and represents the lifecycle states of provisioning a bundle
/// (readouts, preparing, provisioning, etc.)
pub struct Task<'a, B, L, U> {
    model: Arc<Model>,
    conf: &'a Config,
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
    /// Create a new task
    ///
    /// Arguments:
    /// - `model` - the model (states) of the application
    ///   Shared between the task, the UI (`View`) and the input processing (`Input`), i.e.
    ///   the task modifies the model, the UI renders the model and the input processing triggers model changes on terminal resize events (MVC)
    /// - `conf` - the configuration of the task
    /// - `bundle_base_loader` - An optional loader used to load the base bundle; the base bundle (if used)
    ///   usually contains the device-independent payloads like the bootloader, the partition image
    ///   and the factory app image
    /// - `bundle_loader` - The loader used to load the bundle; in case `bundle_base_loader` is used, this
    ///   loader is used to load the device-specific payloads like the NVS partitions. The two bundles are then merged
    /// - `bundle_logs_uploader` - The uploader used to upload the logs from the device provisioning to the server
    pub fn new(
        model: Arc<Model>,
        conf: &'a Config,
        bundle_base_loader: Option<B>,
        bundle_loader: L,
        bundle_logs_uploader: U,
    ) -> Self {
        Self {
            model,
            conf,
            bundle_base_loader,
            bundle_loader,
            bundle_logs_uploader,
        }
    }

    /// Run the factory bundle provisioning task in a loop as follows:
    /// - Step 1: eFuse readouts (read the necessary IDs from the chip eFuse memory)
    /// - Step 2: Readouts (read the necessary IDs from the user, e.g. Device ID, PCB ID, Test JIG ID)
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
    pub async fn run(&mut self, input: impl TaskInput + Clone) -> anyhow::Result<()> {
        let result = self.step(input).await;

        match result {
            Err(TaskError::Quit) => {
                info!("Quit by user request");
                Ok(())
            }
            Err(TaskError::Other(err)) => Err(err)?,
            Ok(_) | Err(TaskError::Canceled) | Err(TaskError::Retry) | Err(TaskError::Skipped) => {
                unreachable!(
                    "Task canceled/retried/skipped by user request: {:?}",
                    result
                );
            }
        }
    }

    async fn step(&mut self, mut input: impl TaskInput + Clone) -> Result<(), TaskError> {
        loop {
            {
                self.model.modify(|inner| {
                    inner.logs.clear().unwrap();
                });
            }

            info!("========== Starting PCB provisioning ==========");

            let _guard = {
                let model = self.model.clone();

                scopeguard::guard((), move |_| {
                    model.access_mut(|inner| {
                        inner.logs.file.grab();
                        ((), false)
                    });
                })
            };

            let (bundle_id, bundle_name, summary) = 'steps: loop {
                let mut readouts = Vec::new();

                let bundle_id = loop {
                    readouts.clear();

                    let mut add_readouts = |new_readouts: &[(String, String)], fill_test_jig| {
                        for (name, value) in new_readouts {
                            readouts.push((name.clone(), value.clone()));
                        }

                        if fill_test_jig
                            && !self.conf.test_jig_id_readout
                            && !self.conf.test_jig_id.is_empty()
                        {
                            readouts
                                .push(("Test JIG ID".to_string(), self.conf.test_jig_id.clone()));
                        }
                    };

                    info!("=== => STEP 1: manual readouts");

                    let result = self.step1_readout(&mut input).await;

                    match result {
                        Ok(_) => (),
                        Err(TaskError::Canceled) => continue,
                        Err(TaskError::Retry) => unreachable!(),
                        Err(other) => Err(other)?,
                    }

                    self.model.access(|inner| {
                        add_readouts(&inner.state.readout().readouts, true);
                    });

                    info!("=== => STEP 2: eFuse readouts");

                    let err_policy = if self.conf.efuse_ignore_failed_readouts {
                        ErrPolicy::Ignore
                    } else {
                        ErrPolicy::ExplicitIgnore
                    };

                    let efuse_values = loop {
                        let result = Self::handle(
                            &self.model.clone(),
                            self.step2_prepare_efuse_readout(input.clone()),
                            "Preparing eFuse readouts failed",
                            err_policy,
                            &mut input,
                        )
                        .await;

                        break match result {
                            Ok(new_readouts) => new_readouts,
                            Err(TaskError::Skipped) => {
                                vec![("EFUSE_READOUT_FAILED".to_string(), "Y".to_string())]
                            }
                            Err(TaskError::Retry) => continue,
                            Err(TaskError::Canceled) => continue 'steps,
                            Err(other) => Err(other)?,
                        };
                    };

                    add_readouts(&efuse_values, false);

                    break loop {
                        info!("=== => STEP 3: Bundle preparation");

                        let result = Self::handle(
                            &self.model.clone(),
                            self.step3_prepare(input.clone(), &readouts),
                            "Preparing a bundle failed",
                            ErrPolicy::Propagate,
                            &mut input,
                        )
                        .await;

                        match result {
                            Ok(bundle_id) => break bundle_id,
                            Err(TaskError::Canceled) => continue 'steps,
                            Err(TaskError::Retry) => continue,
                            Err(other) => Err(other)?,
                        };
                    };
                };

                self.model.access_mut(|inner| {
                    inner.state.provision_mut().readouts = readouts.clone();

                    ((), true)
                });

                break loop {
                    info!("=== => STEP 4: PCB provisioning");

                    if !self.conf.skip_confirmations {
                        match input
                            .confirm("Provision? <[Y]es/ENTER, [N]o/[C]ancel, [Q]uit>")
                            .await
                            .into()
                        {
                            Ok(_) => (),
                            Err(TaskError::Canceled) => continue 'steps,
                            Err(TaskError::Retry) => unreachable!(),
                            Err(other) => Err(other)?,
                        }
                    }

                    // TODO: Not very efficient
                    let provision = self.model.access(|inner| inner.state.provision().clone());

                    let result = Self::handle(
                        &self.model.clone(),
                        self.step4_provision(input.clone()),
                        &format!("Provisioning bundle `{}` failed", provision.bundle.name),
                        ErrPolicy::Propagate,
                        &mut input,
                    )
                    .await;

                    let (bundle_name, chip) = match result {
                        Ok((bundle_name, chip)) => (bundle_name, chip),
                        Err(TaskError::Canceled) => continue 'steps,
                        Err(TaskError::Retry) => {
                            self.model
                                .modify(|inner| inner.state = State::Provision(provision));

                            continue;
                        }
                        Err(other) => Err(other)?,
                    };

                    let result = Self::handle(
                        &self.model.clone(),
                        self.step5_run_app(bundle_name.clone(), chip, input.clone()),
                        &format!("Running app from bundle `{}` failed", bundle_name),
                        ErrPolicy::Propagate,
                        &mut input,
                    )
                    .await;

                    match result {
                        Ok(_) => (),
                        Err(TaskError::Canceled) => continue 'steps,
                        Err(TaskError::Retry) => {
                            self.model
                                .modify(|inner| inner.state = State::Provision(provision));

                            continue;
                        }
                        Err(other) => Err(other)?,
                    };

                    break (bundle_id, bundle_name, readouts);
                };
            };

            info!("========== PCB provisioning complete, uploading logs ==========");

            let log_file = self
                .model
                .access_mut(|inner| (inner.logs.file.grab(), true));

            if let Some(log_file) = log_file {
                let log = FileLogs::finish(log_file, &summary)?;
                self.bundle_logs_uploader
                    .upload_logs(log, bundle_id.as_deref(), &bundle_name)
                    .await?;
            }

            if !self.conf.skip_confirmations
                && matches!(
                    input.confirm("Continue? <Any key, [Q]uit>").await,
                    TaskConfirmationOutcome::Quit
                )
            {
                break;
            }
        }

        Ok(())
    }

    /// Step 1:
    /// Process the readouts state by visualizing the eFuse readouts (if any) and
    /// reading the necessary IDs from the user (if any)
    async fn step1_readout(&mut self, mut input: impl TaskInput) -> Result<(), TaskError> {
        if !self.conf.test_jig_id.is_empty() {
            if self.conf.test_jig_id_readout {
                warn!(
                    "Ignoring the fixed value `{}` for the Test JIG ID from the configuration",
                    self.conf.test_jig_id
                );
            } else {
                info!("Readout `Test JIG ID`: `{}`", self.conf.test_jig_id);
            }
        }

        let init = |readouts: &mut Readout| {
            readouts.readouts.clear();
            readouts.active = 0;

            if self.conf.device_id_readout {
                readouts
                    .readouts
                    .push(("Device ID".to_string(), "".to_string()));
            }

            if self.conf.pcb_id_readout {
                readouts
                    .readouts
                    .push(("PCB ID".to_string(), "".to_string()));
            }

            if self.conf.test_jig_id_readout {
                readouts
                    .readouts
                    .push(("Test JIG ID".to_string(), "".to_string()));
            }
        };

        self.model.modify(|inner| {
            inner.state = State::Readout(Readout::new());
            let readouts = inner.state.readout_mut();
            init(readouts);
        });

        let mut result = Ok(());

        while result.is_ok() && !self.model.access(|inner| inner.state.readout().is_ready()) {
            let (label, value) = self.model.access(|inner| {
                let readouts = inner.state.readout();

                readouts.readouts[readouts.active].clone()
            });

            match input.input(&label, &value).await {
                TaskInputOutcome::Modified(value) => {
                    self.model.modify(|inner| {
                        let readouts = inner.state.readout_mut();
                        readouts.readouts[readouts.active].1 = value;
                    });
                }
                TaskInputOutcome::Done(value) => {
                    self.model.modify(|inner| {
                        let readouts = inner.state.readout_mut();
                        readouts.readouts[readouts.active].1 = value.clone();
                        readouts.active += 1;
                    });

                    info!("Readout `{label}`: `{value}`");
                }
                TaskInputOutcome::StartOver => {
                    let reset = self.model.access_mut(|inner| {
                        let readouts = inner.state.readout_mut();

                        if readouts.active == 0 {
                            let readouts = inner.state.readout_mut();
                            init(readouts);

                            (true, true)
                        } else {
                            readouts.active -= 1;
                            readouts.readouts[readouts.active].1.clear();

                            (false, true)
                        }
                    });

                    if reset {
                        info!("All readouts reset");
                    } else {
                        info!("Readout `{label}` reset");
                    }
                }
                TaskInputOutcome::Quit => {
                    result = Err(TaskError::Quit);
                }
            }
        }

        result
    }

    /// Step 2:
    /// Prepare the eFuse readouts by reading those from the chip eFuse memory
    ///
    /// Displays a progress info while reading the eFuse values
    async fn step2_prepare_efuse_readout(
        &mut self,
        input: impl TaskInput,
    ) -> anyhow::Result<Vec<(String, String)>, TaskError> {
        self.model
            .modify(|inner| inner.state = State::Processing(Processing::new(" Read eFuse IDs ")));

        Self::process(&self.model.clone(), self.prep_efuse_readouts(), input).await
    }

    /// Step 3:
    /// Prepare the bundle to be provisioned by loading it from the storage
    async fn step3_prepare(
        &mut self,
        input: impl TaskInput,
        readouts: &[(String, String)],
    ) -> anyhow::Result<Option<String>, TaskError> {
        let (device_id, pcb_id, _test_jig_id) = {
            let mut offset = 0;

            let device_id = if self.conf.device_id_readout {
                let readout = readouts[offset].1.clone();

                offset += 1;

                Some(readout)
            } else {
                None
            };

            let pcb_id = if self.conf.pcb_id_readout {
                let readout = readouts[offset].1.clone();

                offset += 1;

                Some(readout)
            } else {
                None
            };

            let test_jig_id = if self.conf.test_jig_id_readout {
                let readout = readouts[offset].1.clone();

                Some(readout)
            } else {
                None
            };

            (device_id, pcb_id, test_jig_id)
        };

        self.model
            .modify(|inner| inner.state = State::Processing(Processing::new(" Preparing bundle ")));

        let bundle_id_source = match &self.conf.bundle_identification {
            BundleIdentification::None => None,
            BundleIdentification::DeviceId(parsing) => {
                device_id.map(|device_id| (device_id, parsing))
            }
            BundleIdentification::PcbId(parsing) => pcb_id.map(|pcb_id| (pcb_id, parsing)),
        };

        let bundle_id = bundle_id_source
            .as_ref()
            .map(|(id, parsing)| parsing.parse(id))
            .transpose()
            .map_err(TaskError::Other)?;

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
    async fn step4_provision(
        &mut self,
        mut input: impl TaskInput,
    ) -> anyhow::Result<(String, Chip), TaskError> {
        match select(self.prov_bundle(), input.swallow()).await {
            Either::First(result) => result.map_err(TaskError::Other),
        }
    }

    /// Step 5:
    /// Optionally, run the app
    async fn step5_run_app(
        &mut self,
        bundle_name: String,
        chip: Chip,
        mut input: impl TaskInput,
    ) -> anyhow::Result<(), TaskError> {
        match select(self.run_app(bundle_name, chip), input.swallow()).await {
            Either::First(result) => result.map_err(TaskError::Other),
        }
    }

    //
    // Helper methods
    //

    /// Prepare the eFuse readouts by reading those from the chip eFuse memory
    async fn prep_efuse_readouts(&mut self) -> anyhow::Result<Vec<(String, String)>> {
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

        self.model.modify(|inner| {
            inner.state.processing_mut().status = "Reading Chip IDs from eFuse".to_string();
        });

        info!("About to read Chip IDs from eFuse");

        let efuse_port = self.conf.port.clone();
        let efuse_baud = self.conf.efuse_speed.map(|speed| speed.to_string());

        let efuse_values = unblock("efuse-summary", move || {
            let efuse_values = efuse::summary(
                None,
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

        Ok(efuse_values)
    }

    /// Prepare the bundle to be provisioned by creating a `Bundle` instance from the loaded bundle content
    /// in the bundle workspace directory
    async fn prep_bundle(&mut self, bundle_id: Option<&str>) -> anyhow::Result<()> {
        let bundle = Self::prep_one_bundle(
            &self.model,
            bundle_id,
            &mut self.bundle_loader,
            self.bundle_base_loader.is_none() && self.conf.supply_default_partition_table,
            self.bundle_base_loader.is_none() && self.conf.supply_default_bootloader,
        )
        .await?;

        let mut bundle = if let Some(base_loader) = self.bundle_base_loader.as_mut() {
            info!("About to load base bundle");

            let mut base_bundle = Self::prep_one_bundle(
                &self.model,
                None,
                base_loader,
                self.conf.supply_default_partition_table,
                self.conf.supply_default_bootloader,
            )
            .await?;

            info!("Loaded base bundle `{}`", base_bundle.name);

            self.model.modify(|inner| {
                inner.state.processing_mut().status =
                    format!("Merging `{}` and `{}`", base_bundle.name, bundle.name);
            });

            info!(
                "Merging base bundle `{}` with bundle `{}`, override `{}`",
                base_bundle.name, bundle.name, self.conf.overwrite_on_merge
            );

            base_bundle.add(bundle, self.conf.overwrite_on_merge)?;

            info!("Bundles merged");

            base_bundle
        } else {
            bundle
        };

        if self.conf.reset_empty_partitions {
            info!("Adding 0xff images for empty partitions");

            bundle.add_empty();
        }

        self.model.modify(move |inner| {
            inner.state = State::Provision(Provision {
                readouts: Vec::new(),
                bundle,
                provisioning: false,
            })
        });

        Ok(())
    }

    /// Provision the bundle by flashing and optionally efusing the chip with the bundle content
    async fn prov_bundle(&mut self) -> anyhow::Result<(String, Chip)> {
        let bundle_name = self.model.modify(|inner| {
            let ps = inner.state.provision_mut();
            ps.provisioning = true;

            ps.bundle.set_status_all(ProvisioningStatus::Pending);

            ps.bundle.name.clone()
        });

        info!("About to provision bundle `{bundle_name}`");

        let flash_erase_all = self.conf.flash_erase;

        let (chip, flash_size, keys, mut flash_data) = self.model.access(|inner| {
            let ps = inner.state.provision();

            (
                ps.bundle.params.chip,
                ps.bundle.params.flash_size,
                ps.bundle
                    .get_flash_encrypt_keys()
                    .map(|key| key.to_vec())
                    .collect::<Vec<_>>(),
                ps.bundle.get_flash_data().collect::<Vec<_>>(),
            )
        });

        if self.conf.flash_encrypt && flash_data.iter().any(|fd| fd.encrypted_partition) {
            let key = if keys.is_empty() {
                anyhow::bail!("No encryption keys provided for flash data");
            } else if keys.len() > 1 {
                anyhow::bail!("Multiple encryption keys provided for flash data");
            } else {
                &keys[0]
            };

            info!(
                "About to ENCRYPT flash data: Chip={chip:?}, Flash Size={flash_size:?}, Images N={}",
                flash_data.len()
            );

            for flash_data in &mut flash_data {
                if flash_data.encrypted_partition {
                    info!(
                        "Encrypting image for addr `0x{:08x}`, {}B",
                        flash_data.offset,
                        flash_data.data.len()
                    );

                    let encrypted_data = {
                        let offset = flash_data.offset;
                        let raw_data = flash_data.data.clone();
                        let key = key.clone();

                        unblock("encrypt-flash-data", move || {
                            encrypt(offset as _, &raw_data, &key)
                        })
                        .await?
                    };

                    flash_data.data = Arc::new(encrypted_data);
                }
            }
        }

        if flash_erase_all {
            info!("About to erase all flash using the standard `Flash Erase` command: Chip={chip:?}, Flash Size={flash_size:?}");
        }

        info!(
            "About to flash data: Chip={chip:?}, Flash Size={flash_size:?}, Images N={}",
            flash_data.len()
        );

        let flash_use_stub = !self.conf.flash_no_stub;
        let flash_esptool = self.conf.flash_esptool;
        let flash_port = self.conf.port.clone();
        let flash_speed = self.conf.flash_speed;
        let flash_model = self.model.clone();
        let flash_dry_run = self.conf.flash_dry_run;

        unblock("flash", move || {
            let mut progress = FlashProgress::new(flash_model);

            if flash_esptool {
                if flash_erase_all {
                    flash::erase_esptool(
                        flash_port.as_deref(),
                        chip,
                        flash_use_stub,
                        flash_speed,
                        flash_size,
                        flash_dry_run,
                    )?;
                }

                flash::flash_esptool(
                    flash_port.as_deref(),
                    chip,
                    flash_use_stub,
                    flash_speed,
                    flash_size,
                    flash_data,
                    flash_dry_run,
                    &mut progress,
                )
            } else {
                if flash_erase_all {
                    flash::erase(
                        flash_port.as_deref(),
                        chip,
                        flash_use_stub,
                        flash_speed,
                        flash_size,
                        flash_dry_run,
                    )?;
                }

                flash::flash(
                    flash_port.as_deref(),
                    chip,
                    flash_use_stub,
                    flash_speed,
                    flash_size,
                    flash_data,
                    flash_dry_run,
                    &mut progress,
                )
            }
        })
        .await?;

        info!("Flash complete");

        info!("About to burn eFuses");

        let model = self.model.clone();

        let efuse_protect_keys = self.conf.efuse_protect_keys;
        let efuse_protect_digests = self.conf.efuse_protect_digests;
        let efuse_port = self.conf.port.clone();
        let efuse_baud = self.conf.efuse_speed.map(|speed| speed.to_string());
        let efuse_dry_run = self.conf.efuse_dry_run;

        unblock("efuse-burn", move || {
            Self::burn(
                &model,
                efuse_protect_keys,
                efuse_protect_digests,
                chip,
                efuse_port.as_deref(),
                efuse_baud.as_deref(),
                efuse_dry_run,
            )
        })
        .await?;

        info!("Burn complete");

        info!("Provisioning bundle `{bundle_name}` complete");

        Ok((bundle_name, chip))
    }

    async fn run_app(&mut self, bundle_name: String, chip: Chip) -> anyhow::Result<()> {
        if !matches!(self.conf.app_run, AppRun::Disabled) {
            info!("Running app to finish provisioning");

            self.model.modify(|inner| {
                inner.state = State::AppRun(AppLogs::new(100));
            });

            let run_use_stub = !self.conf.flash_no_stub;
            let run_port = self.conf.port.clone();
            let run_speed = self.conf.flash_speed;
            let run_model = Arc::new(Mutex::new(Some(self.model.clone())));
            let run_model_inner = run_model.clone();
            let run_stop = Arc::new(AtomicBool::new(false));
            let run_stop_inner = run_stop.clone();
            let (run_end_regex, run_timeout_secs) = match &self.conf.app_run {
                AppRun::MatchPattern {
                    pattern,
                    timeout_secs,
                } => (
                    Some(regex::Regex::new(pattern).context("Invalid regex pattern")?),
                    *timeout_secs,
                ),
                AppRun::ForSecs { secs } => (None, *secs),
                _ => unreachable!(),
            };
            let run_end_regex_present = run_end_regex.is_some();

            let mut log_task = pin!(unblock("run-app", move || {
                flash::run_app_esptool(run_port.as_deref(), chip, run_use_stub, run_speed)?;

                info!("APP LOG START >>>>>>>>>>>>>>>>>>>>>>>>>>");

                monitor::monitor(
                    run_port.as_deref(),
                    None,
                    DEFAULT_BAUD_RATE,
                    LogFormat::Serial,
                    false,
                    run_stop_inner.clone(),
                    LineWrite::new(move |line| {
                        let model = run_model_inner.lock().unwrap();

                        if let Some(model) = model.as_ref() {
                            // Strip the ANSI escape sequences
                            // TODO: Do something more intelligent in the future
                            //let line = line.chars().filter(|c| *c >= ' ').collect::<String>();
                            let line = strip_ansi_escapes::strip_str(line);

                            info!("[APP LOG] {line}");

                            model.modify(|inner| {
                                inner.state.app_logs_mut().append(line.clone());
                            });

                            if let Some(regex) = run_end_regex.as_ref() {
                                if regex.is_match(&line) {
                                    run_stop_inner.store(true, Ordering::SeqCst);
                                    info!("[App run finishing, detected pattern on this line ^^^]");
                                }
                            }
                        }
                    }),
                )?;

                info!("APP LOG END <<<<<<<<<<<<<<<<<<<<<<<<<<<<");

                Ok(())
            }));

            let mut timeout_task = pin!(embassy_time::Timer::after(Duration::from_secs(
                run_timeout_secs as _
            )));

            let result = select(&mut log_task, &mut timeout_task).await;

            run_stop.store(true, Ordering::SeqCst);
            *run_model.lock().unwrap() = None;

            match result {
                Either::First(result) => {
                    result?;

                    info!("App run auccessful, match pattern detected");
                }
                Either::Second(_) => {
                    if run_end_regex_present {
                        error!("App run timeout after {run_timeout_secs} seconds");
                        anyhow::bail!("App run timeout after {run_timeout_secs} seconds");
                    } else {
                        info!("App run auccessful, timeout reached");
                    }
                }
            }
        } else {
            info!("App run disabled");
        }

        self.model.modify(|inner| {
            inner
                .state
                .success(format!(" {bundle_name} "), "Provisioning complete.");
        });

        Ok(())
    }

    /// Load a bundle from the storage of the bundle loader into the bundle workspace directory
    async fn load_one_bundle<T>(
        model: &Model,
        bundle_id: Option<&str>,
        mut loader: T,
    ) -> anyhow::Result<(String, NamedTempFile)>
    where
        T: BundleLoader,
    {
        model.modify(|inner| {
            inner.state.processing_mut().status = "Fetching".into();
        });

        let mut bundle_file = NamedTempFile::new().context("Creating temp bundle file failed")?;
        let bundle_name = loader.load(&mut bundle_file, bundle_id).await?;

        bundle_file
            .flush()
            .context("Flushing the temp bundle file failed")?;

        info!(
            "Bundle `{bundle_name}` loaded into file `{}`",
            bundle_file.path().display()
        );

        Ok((bundle_name, bundle_file))
    }

    /// Prepare a bundle to be provisioned by creating a `Bundle` instance from the loaded bundle content
    /// in the bundle workspace directory
    async fn prep_one_bundle<T>(
        model: &Model,
        bundle_id: Option<&str>,
        loader: T,
        supply_default_partition_table: bool,
        supply_default_bootloader: bool,
    ) -> anyhow::Result<Bundle>
    where
        T: BundleLoader,
    {
        let (bundle_name, mut bundle_file) =
            Self::load_one_bundle(model, bundle_id, loader).await?;

        model.modify(|inner| {
            inner.state.processing_mut().status = format!("Processing {bundle_name}");
        });

        info!(
            "About to prep bundle file `{}`",
            bundle_file.path().display()
        );

        bundle_file
            .seek(std::io::SeekFrom::Start(0))
            .context("Seeking the loaded bundle file failed")?;

        let bundle = Bundle::create(
            bundle_name,
            Params::default(),
            &mut bundle_file,
            supply_default_partition_table,
            supply_default_bootloader,
        )?;

        info!("{bundle}");

        Ok(bundle)
    }

    fn burn(
        model: &Model,
        protect_keys: bool,
        protect_digests: bool,
        chip: Chip,
        port: Option<&str>,
        baud: Option<&str>,
        dry_run: bool,
    ) -> anyhow::Result<String> {
        let mut output = String::new();

        model.modify(|inner| {
            let efuses = &mut inner.state.provision_mut().bundle.efuse_mapping;

            for efuse in efuses {
                efuse.status = ProvisioningStatus::Pending;
            }
        });

        // Step 1: Burn keys first

        let keys = model.access_mut(|inner| {
            let efuses = &mut inner.state.provision_mut().bundle.efuse_mapping;

            let mut notify = false;

            let mut keys = Vec::new();
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
                notify = true;
            }

            (keys, notify)
        });

        if !keys.is_empty() {
            info!("Initiating burn of {} keys", keys.len());

            let keys_output = efuse::burn_keys(
                protect_keys,
                chip,
                port,
                baud,
                dry_run,
                keys.iter().map(|(block, key, purpose)| {
                    (block.as_str(), key.as_slice(), purpose.as_str())
                }),
            )
            .context("Burning keys failed")?;

            model.modify(|inner| {
                let efuses = &mut inner.state.provision_mut().bundle.efuse_mapping;

                for efuse in efuses {
                    if let Efuse::Key { .. } = &efuse.efuse {
                        efuse.status = ProvisioningStatus::Done;
                    }
                }
            });

            write!(&mut output, "{keys_output}\n\n")?;

            info!("Burn of keys complete");
        }

        // Step 2: Burn key digests next (should be after keys, check the comment inside `efuse::burn_keys_or_digests`)

        let digests = model.access_mut(|inner| {
            let efuses = &mut inner.state.provision_mut().bundle.efuse_mapping;

            let mut notify = false;

            let mut digests = Vec::new();

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
                notify = true;
            }

            (digests, notify)
        });

        if !digests.is_empty() {
            info!("Initiating burn of {} key digests", digests.len());

            let digests_output = efuse::burn_key_digests(
                protect_digests,
                chip,
                port,
                baud,
                dry_run,
                digests.iter().map(|(block, digest, purpose)| {
                    (block.as_str(), digest.as_slice(), purpose.as_str())
                }),
            )
            .context("Burning key digests failed")?;

            model.modify(|inner| {
                let efuses = &mut inner.state.provision_mut().bundle.efuse_mapping;

                for efuse in efuses {
                    if let Efuse::KeyDigest { .. } = &efuse.efuse {
                        efuse.status = ProvisioningStatus::Done;
                    }
                }
            });

            write!(&mut output, "{digests_output}\n\n")?;

            info!("Burn of key digests complete");
        }

        // Step 3: Finally, burn all params

        let params = model.access_mut(|inner| {
            let efuses = &mut inner.state.provision_mut().bundle.efuse_mapping;

            let mut notify = false;

            let mut params = Vec::new();

            for efuse in efuses {
                if let Efuse::Param { name, value } = &efuse.efuse {
                    params.push((name.clone(), *value));
                }

                efuse.status = ProvisioningStatus::Pending;
                notify = true;
            }

            (params, notify)
        });

        if !params.is_empty() {
            info!("Initiating burn of {} params", params.len());

            let params_output = efuse::burn_efuses(
                chip,
                port,
                baud,
                dry_run,
                params.iter().map(|(name, value)| (name.as_str(), *value)),
            )
            .context("Burning params failed")?;

            model.modify(|inner| {
                let efuses = &mut inner.state.provision_mut().bundle.efuse_mapping;

                for efuse in efuses {
                    if let Efuse::Param { .. } = &efuse.efuse {
                        efuse.status = ProvisioningStatus::Done;
                    }
                }
            });

            write!(&mut output, "{params_output}\n\n")?;

            info!("Burn of params complete");
        }

        Ok(output)
    }

    /// Handle a future failure by displaying an error message and waiting for a confirmation
    async fn handle<F, R>(
        model: &Model,
        fut: F,
        err_msg: &str,
        err_policy: ErrPolicy,
        mut input: impl TaskInput,
    ) -> anyhow::Result<R, TaskError>
    where
        F: Future<Output = anyhow::Result<R, TaskError>>,
    {
        let result = fut.await;

        if let Err(TaskError::Other(err)) = result {
            error!("{err_msg}: {err:?}");

            model.modify(|inner| {
                inner
                    .state
                    .error(format!(" {err_msg} "), format!("{err_msg}: {err:?}"))
            });

            match err_policy {
                ErrPolicy::Ignore => {
                    info!("Ignoring the error");

                    Err(TaskError::Skipped)
                }
                ErrPolicy::ExplicitIgnore => {
                    match input
                        .confirm_or_skip("Retry? <[Y]es/ENTER, [N]o/[C]ancel, [I]gnore, [Q]uit")
                        .await
                        .into()
                    {
                        Ok(_) => Err(TaskError::Retry),
                        Err(err) => Err(err),
                    }
                }
                _ => {
                    match input
                        .confirm("Retry? <[Y]es/ENTER, [N]o/[C]ancel, [Q]uit")
                        .await
                        .into()
                    {
                        Ok(_) => Err(TaskError::Retry),
                        Err(err) => Err(err),
                    }
                }
            }
        } else {
            result
        }
    }

    /// Process a future by incrementing a counter every 100 ms while the future is running
    async fn process<F, R>(
        model: &Model,
        fut: F,
        mut input: impl TaskInput,
    ) -> anyhow::Result<R, TaskError>
    where
        F: Future<Output = anyhow::Result<R>>,
    {
        let result = select3(
            Self::tick(Duration::from_millis(100), || {
                model.modify(|inner| {
                    if let State::Processing(Processing { counter, .. }) = &mut inner.state {
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

        self.model.access_mut(|inner| {
            let notify = inner
                .state
                .provision_mut()
                .bundle
                .set_status(addr, ProvisioningStatus::InProgress(None));

            ((), notify)
        });

        info!(
            "Initiated flash for addr `0x{addr:08x}`, size {}KB",
            total / 1024
        );
    }

    fn update(&mut self, current: usize) {
        if let Some((addr, total)) = *self.image.lock().unwrap() {
            self.model.access_mut(|inner| {
                let notify = inner.state.provision_mut().bundle.set_status(
                    addr,
                    ProvisioningStatus::InProgress(Some((current * 100 / total) as u8)),
                );

                ((), notify)
            });
        }
    }

    fn finish(&mut self) {
        if let Some((addr, _)) = self.image.lock().unwrap().take() {
            self.model.access_mut(|inner| {
                let notify = inner
                    .state
                    .provision_mut()
                    .bundle
                    .set_status(addr, ProvisioningStatus::Done);

                ((), notify)
            });

            info!("Flash for addr `0x{addr:08x}` completed");
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
    /// Step skipped by user request
    Skipped,
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

impl From<TaskConfirmationOutcome> for Result<(), TaskError> {
    fn from(outcome: TaskConfirmationOutcome) -> Self {
        match outcome {
            TaskConfirmationOutcome::Confirmed => Ok(()),
            TaskConfirmationOutcome::Canceled => Err(TaskError::Canceled),
            TaskConfirmationOutcome::Skipped => Err(TaskError::Skipped),
            TaskConfirmationOutcome::Quit => Err(TaskError::Quit),
        }
    }
}

impl Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskError::Retry => write!(f, "Retry"),
            TaskError::Canceled => write!(f, "Canceled"),
            TaskError::Skipped => write!(f, "Skipped"),
            TaskError::Quit => write!(f, "Quit"),
            TaskError::Other(err) => write!(f, "Error: {err:#}"),
        }
    }
}

impl std::error::Error for TaskError {}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
enum ErrPolicy {
    Propagate,
    ExplicitIgnore,
    Ignore,
}
