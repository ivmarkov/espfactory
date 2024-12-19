use std::fs::{self, DirEntry, File};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use alloc::sync::Arc;

use anyhow::Context;

use crossterm::event::KeyCode;

use embassy_futures::select::{select, select3, Either};
use embassy_time::{Duration, Ticker};

use espflash::flasher::ProgressCallbacks;

use log::{error, info};

use crate::bundle::{Bundle, Params, ProvisioningStatus};
use crate::input::Input;
use crate::loader::BundleLoader;
use crate::model::{Model, Prepared, Preparing, Provisioning, Readouts, State, Status};
use crate::utils::futures::{unblock, Coalesce, IntoFallibleFuture};
use crate::{efuse, flash};
use crate::{BundleIdentification, Config};

extern crate alloc;

/// A task that runs the factory application and represents the lifecycle states of provisioning a bundle
/// (readouts, preparing, provisioning, etc.)
pub struct Task<'a, T> {
    model: Arc<Model>,
    conf: &'a Config,
    bundle_dir: &'a Path,
    bundle_loader: T,
}

impl<'a, T> Task<'a, T>
where
    T: BundleLoader,
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
    /// - `bundle_loader` - the loader used to load the bundles
    pub fn new(
        model: Arc<Model>,
        conf: &'a Config,
        bundle_dir: &'a Path,
        bundle_loader: T,
    ) -> Self {
        Self {
            model,
            conf,
            bundle_dir,
            bundle_loader,
        }
    }

    /// Run the factory bundle provisioning task in a loop as follows:
    /// - Readouts (read the necessary IDs from the user, e.g. test jig ID, PCB ID, box ID)
    /// - Load and prepare the (next) bundle to be provisioned, possibly using one of the readouts as a bundle ID
    ///   by fetching the bundle content using the bundle loader, and then creating a `Bundle` instance
    /// - Provision the bundle by flashing and optionally efusing the chip with the bundle content
    /// - Repeat the above steps until the user quits
    ///
    /// Arguments:
    /// - `input` - the input helper to process terminal events
    ///   Necessary as some states require direct user input (e.g. readouts)
    pub async fn run(&mut self, input: &mut Input<'_>) -> anyhow::Result<()> {
        loop {
            self.prepare_efuse_readouts(input).await?;

            loop {
                if !self.readout(input).await {
                    return Ok(());
                }

                match self.prepare(input).await {
                    Ok(()) => break,
                    Err(err) => {
                        error!("Preparing a bundle failed: {err:?}");

                        self.model.modify(|state| {
                            *state = State::ProvisioningOutcome(Status {
                                title: " Preparing a bundle failed ".to_string(),
                                message: format!("Preparing a bundle failed: {err:?}"),
                                error: true,
                            });
                        });

                        if !input.wait_quit_or(KeyCode::Enter).await {
                            return Err(err);
                        }
                    }
                }
            }

            loop {
                if !self.conf.skip_confirmations && !input.wait_quit_or(KeyCode::Enter).await {
                    break;
                }

                match self.provision(input).await {
                    Ok(()) => break,
                    Err(err) => {
                        error!("Provisioning the bundle failed: {err:?}");

                        let mut prepared_bundle = None;

                        self.model.modify(|state| {
                            let prev_state = core::mem::replace(
                                state,
                                State::ProvisioningOutcome(Status {
                                    title: format!(
                                        " Provisioning {} failed ",
                                        state.provisioning().bundle.name
                                    ),
                                    message: format!("Provisioning the bundle failed: {err:?}"),
                                    error: true,
                                }),
                            );

                            let State::Provisioning(Provisioning { bundle, .. }) = prev_state
                            else {
                                unreachable!();
                            };

                            prepared_bundle = Some(bundle);
                        });

                        if !input.wait_quit_or(KeyCode::Enter).await {
                            return Err(err);
                        }

                        self.model.modify(|state| {
                            *state = State::Prepared(Prepared {
                                bundle: prepared_bundle.unwrap(),
                            });
                        });
                    }
                }
            }

            if !self.conf.skip_confirmations && !input.wait_quit_or(KeyCode::Enter).await {
                break;
            }
        }

        Ok(())
    }

    /// Process the readouts state by visualizing the eFuse readouts (if any) and
    /// reading the necessary IDs from the user (if any)
    async fn readout(&mut self, input: &mut Input<'_>) -> bool {
        let init = |readouts: &mut Readouts| {
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
            let readouts = state.readouts_mut();
            init(readouts);
        });

        while !self.model.get(|state| state.readouts().is_ready()) {
            let key = input.get().await;

            let mut quit = false;

            self.model.maybe_modify(|state| {
                let readouts = state.readouts_mut();

                let readout = &mut readouts.readouts[readouts.active];

                match key {
                    KeyCode::Enter => {
                        if !readout.1.is_empty() {
                            readouts.active += 1;
                            info!("Readout `{}`: `{}`", readout.0, readout.1);
                            return true;
                        }
                    }
                    KeyCode::Esc => {
                        if readouts.active == 0 && readout.1.is_empty() {
                            quit = true;
                            return false;
                        }

                        init(readouts);
                        info!("Readouts reset");
                        return true;
                    }
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

                false
            });

            if quit {
                return false;
            }
        }

        true
    }

    /// Prepare the eFuse readouts by reading those from the chip eFuse memory
    ///
    /// Displays a progress info while reading the eFuse values
    async fn prepare_efuse_readouts(&mut self, input: &mut Input<'_>) -> anyhow::Result<()> {
        self.model
            .modify(|state| *state = State::PreparingEfuseReadouts(Preparing::new()));

        let model = self.model.clone();

        select3(
            Self::tick(Duration::from_millis(100), || {
                model.modify(|state| {
                    if let State::PreparingEfuseReadouts(Preparing { counter, .. }) = state {
                        *counter += 1;
                    }
                })
            })
            .into_fallible(),
            input.wait_quit().into_fallible(),
            self.prep_efuse_readouts(),
        )
        .coalesce()
        .await
    }

    /// Prepare the bundle to be provisioned by loading it from the storage
    async fn prepare(&mut self, input: &mut Input<'_>) -> anyhow::Result<()> {
        let (_test_jig_id, pcb_id, box_id) = self.model.get(|state| {
            let readouts = state.readouts();
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
            .modify(|state| *state = State::Preparing(Preparing::new()));

        let model = self.model.clone();

        let bundle_id = match self.conf.bundle_identification {
            BundleIdentification::None => None,
            BundleIdentification::PcbId => pcb_id,
            BundleIdentification::BoxId => box_id,
        };

        select3(
            Self::tick(Duration::from_millis(100), || {
                model.modify(|state| {
                    if let State::Preparing(Preparing { counter, .. }) = state {
                        *counter += 1;
                    }
                })
            })
            .into_fallible(),
            input.wait_quit().into_fallible(),
            self.prep_bundle(bundle_id.as_deref()),
        )
        .coalesce()
        .await
    }

    /// Load the bundle from the storage of the bundle loader into the bundle workspace directory
    async fn load_bundle(&mut self, bundle_id: Option<&str>) -> anyhow::Result<PathBuf> {
        let bundle = loop {
            self.model.modify(|state| {
                state.preparing_mut().status = "Checking".into();
            });

            if bundle_id.is_none() {
                // - Only preserve the loaded bundle if no bundle ID is provided
                //   (i.e. a random bundle is to be downloaded, used, and possibly deleted from the server)
                // - For bundles with a bundle ID, always re-download the bundle using the loader

                let loaded_path = self.bundle_dir.join(Self::BUNDLE_LOADED_DIR_NAME);
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

            self.model.modify(|state| {
                state.preparing_mut().status = "Fetching".into();
            });

            let mut bundle_temp_path = self
                .bundle_dir
                .join(Self::BUNDLE_TEMP_DIR_NAME)
                .join("bundle");
            fs::create_dir_all(bundle_temp_path.parent().unwrap())
                .context("Creating the temp bundle directory failed")?;

            let bundle_name = {
                let result = {
                    let mut temp_file = File::create(&bundle_temp_path)
                        .context("Creating temp bundle file failed")?;

                    self.bundle_loader.load(&mut temp_file, bundle_id).await
                };

                match result {
                    Ok(bundle_name) => {
                        let bundle_new_temp_path = self.bundle_dir.join(&bundle_name);
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

                let bundle_loaded_path = self
                    .bundle_dir
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
            state.preparing_efuse_mut().status = "Reading Chip IDs from eFuse".to_string();
        });

        info!("About to read Chip IDs from eFuse");

        let efuse_values = unblock("efuse", || {
            let tools = esptools::Tools::mount().context("Mounting esptools failed")?;

            let efuse_values = efuse::summary(&tools, EFUSE_VALUES.iter().copied())?;

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
            .modify(move |state| *state = State::Readouts(Readouts::new_with_efuse(efuse_values)));

        Ok(())
    }

    /// Prepare the bundle to be provisioned by creating a `Bundle` instance from the loaded bundle content
    /// in the bundle workspace directory
    async fn prep_bundle(&mut self, bundle_id: Option<&str>) -> anyhow::Result<()> {
        let bundle_path = self.load_bundle(bundle_id).await?;

        let bundle_name = bundle_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string(); // TODO

        self.model.modify(|state| {
            state.preparing_mut().status = format!("Processing {bundle_name}");
        });

        info!("About to prep bundle file `{}`", bundle_path.display());

        let mut bundle_file =
            File::open(bundle_path).context("Opening the loaded bundle file failed")?;

        let bundle = Bundle::create(bundle_name, Params::default(), &mut bundle_file)?;

        self.model
            .modify(move |state| *state = State::Prepared(Prepared { bundle }));

        Ok(())
    }

    /// Provision the bundle by flashing and optionally efusing the chip with the bundle content
    async fn provision(&mut self, input: &mut Input<'_>) -> anyhow::Result<()> {
        match select(self.prov_bundle(), input.swallow()).await {
            Either::First(result) => result,
        }
    }

    async fn prov_bundle(&mut self) -> anyhow::Result<()> {
        self.model.modify(|state| {
            *state = State::Provisioning(Provisioning {
                bundle: state.prepared_mut().bundle.clone(),
                efuses_status: Default::default(),
            });

            let ps = state.provisioning_mut();
            info!("About to provision bundle `{}`", ps.bundle.name);

            ps.bundle.set_status_all(ProvisioningStatus::Pending);
        });

        let (chip, flash_size, flash_data) = self.model.get(|state| {
            let ps = state.provisioning();

            (
                ps.bundle.params.chip,
                ps.bundle.params.flash_size,
                ps.bundle.get_flash_data().collect::<Vec<_>>(),
            )
        });

        info!(
            "About to flash data:\nChip: {chip:?}\nFlash Size: {flash_size:?}\nImages N: {}",
            flash_data.len()
        );

        let flash_port = self.conf.port.clone();
        let flash_speed = self.conf.flash_speed;
        let flash_model = self.model.clone();

        unblock("flash", move || {
            flash::flash(
                flash_port.as_deref(),
                chip,
                flash_speed,
                flash_size,
                flash_data,
                FlashProgress::new(flash_model),
            )
        })
        .await?;

        info!("Flash complete");

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
                state.provisioning().bundle.name
            );

            *state = State::ProvisioningOutcome(Status {
                title: format!(" {} ", state.provisioning().bundle.name),
                message: "Provisioning complete.".to_string(),
                error: false,
            });
        });

        Ok(())
    }

    async fn tick<F>(duration: Duration, mut f: F)
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

        self.model.maybe_modify(|state| {
            state
                .provisioning_mut()
                .bundle
                .set_status(addr, ProvisioningStatus::InProgress(0))
        });
    }

    fn update(&mut self, current: usize) {
        if let Some((addr, total)) = *self.image.lock().unwrap() {
            self.model.maybe_modify(|state| {
                state.provisioning_mut().bundle.set_status(
                    addr,
                    ProvisioningStatus::InProgress((current * 100 / total) as u8),
                )
            });
        }
    }

    fn finish(&mut self) {
        if let Some((addr, _)) = self.image.lock().unwrap().take() {
            self.model.maybe_modify(|state| {
                state
                    .provisioning_mut()
                    .bundle
                    .set_status(addr, ProvisioningStatus::Done)
            });
        }
    }
}
