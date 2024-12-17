use std::fs::{self, DirEntry, File};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use alloc::sync::Arc;

use crossterm::event::KeyCode;

use embassy_futures::select::select3;
use embassy_time::{Duration, Ticker};

use espflash::flasher::ProgressCallbacks;

use log::{error, info};

use crate::bundle::{Bundle, Params, ProvisioningStatus};
use crate::flash;
use crate::input::Input;
use crate::loader::BundleLoader;
use crate::model::{Model, Prepared, Preparing, Provisioning, Readouts, State, Status};
use crate::utils::futures::{Coalesce, IntoFallibleFuture};
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
            loop {
                self.readout(input).await;

                match self.prepare(input).await {
                    Ok(()) => break,
                    Err(err) => {
                        error!("Preparing a bundle failed: {err}");

                        self.model.modify(|state| {
                            *state = State::ProvisioningOutcome(Status {
                                title: " Preparing a bundle failed ".to_string(),
                                message: err.to_string(),
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
                if !input.wait_quit_or(KeyCode::Enter).await {
                    break;
                }

                match self.provision().await {
                    Ok(()) => break,
                    Err(err) => {
                        error!("Provisioning the bundle failed: {err}");

                        let mut prepared_bundle = None;

                        self.model.modify(|state| {
                            let prev_state = core::mem::replace(
                                state,
                                State::ProvisioningOutcome(Status {
                                    title: format!(
                                        " Provisioning {} failed ",
                                        state.provisioning().bundle.name
                                    ),
                                    message: err.to_string(),
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

            if !input.wait_quit_or(KeyCode::Enter).await {
                break;
            }
        }

        Ok(())
    }

    /// Process the readouts state by reading the necessary IDs from the user
    async fn readout(&mut self, input: &mut Input<'_>) {
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
            let mut readouts = Readouts::new();
            init(&mut readouts);

            *state = State::Readouts(readouts);
        });

        while !self.model.get(|state| state.readouts().is_ready()) {
            let key = input.get().await;

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
        }
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
                fs::create_dir_all(&loaded_path)?;

                let files: Result<Vec<DirEntry>, _> = fs::read_dir(&loaded_path)?.collect();
                let files = files?;

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
            fs::create_dir_all(bundle_temp_path.parent().unwrap())?;

            let bundle_name = {
                let result = {
                    let mut temp_file = File::create(&bundle_temp_path)?;

                    self.bundle_loader.load(&mut temp_file, bundle_id).await
                };

                match result {
                    Ok(bundle_name) => {
                        let bundle_new_temp_path = self.bundle_dir.join(&bundle_name);
                        fs::rename(&bundle_temp_path, &bundle_new_temp_path)?;

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
                fs::create_dir_all(bundle_loaded_path.parent().unwrap())?;

                fs::rename(bundle_temp_path, bundle_loaded_path)?;
            } else {
                break bundle_temp_path;
            }
        };

        Ok(bundle)
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

        let mut bundle_file = File::open(bundle_path)?;

        let bundle = Bundle::create(bundle_name, Params::default(), &mut bundle_file)?;

        self.model
            .modify(move |state| *state = State::Prepared(Prepared { bundle }));

        Ok(())
    }

    /// Provision the bundle by flashing and optionally efusing the chip with the bundle content
    async fn provision(&mut self) -> anyhow::Result<()> {
        self.model.modify(|state| {
            *state = State::Provisioning(Provisioning {
                bundle: state.prepared_mut().bundle.clone(),
                efuses_status: Default::default(),
            });

            let ps = state.provisioning_mut();

            ps.bundle.set_status_all(ProvisioningStatus::Pending);
        });

        let (chip, flash_size, flash_data) = self.model.get(|state| {
            let ps = state.provisioning();

            (
                ps.bundle.params.chip,
                ps.bundle.params.flash_size,
                ps.bundle.get_flash_data().collect(),
            )
        });

        flash::flash(
            self.conf.port.as_deref().unwrap_or("/dev/ttyUSB0"), // TODO
            chip,
            flash_size,
            flash_data,
            FlashProgress::new(self.model.clone()),
        )
        .await?;

        let bundle_loaded_dir = self.bundle_dir.join(Self::BUNDLE_LOADED_DIR_NAME);
        fs::create_dir_all(&bundle_loaded_dir)?;

        for entry in bundle_loaded_dir.read_dir()? {
            let entry = entry?;
            fs::remove_file(entry.path())?;
        }

        self.model.modify(|state| {
            *state = State::ProvisioningOutcome(Status {
                title: format!(" {} ", state.provisioning().bundle.name),
                message: "Provisioning complete".to_string(),
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
