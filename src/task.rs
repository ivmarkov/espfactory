use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use alloc::sync::Arc;

use crossterm::event::KeyCode;

use embassy_futures::select::select3;
use embassy_time::{Duration, Ticker};

use espflash::flasher::ProgressCallbacks;

use zip::ZipArchive;

use crate::bundle::{Bundle, ProvisioningStatus};
use crate::input::Input;
use crate::loader::BundleLoader;
use crate::model::{Model, Prepared, Preparing, Provisioned, Provisioning, Readouts, State};
use crate::utils::futures::{Coalesce, IntoFallibleFuture};
use crate::{flash, BundleIdentification, Config};

extern crate alloc;

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

    pub async fn run(&mut self, input: &mut Input<'_>) -> anyhow::Result<()> {
        loop {
            self.readout(input).await?;

            self.prepare(input).await?;

            if !input.wait_quit_or(KeyCode::Enter).await? {
                break;
            }

            self.provision().await?;

            if !input.wait_quit_or(KeyCode::Enter).await? {
                break;
            }
        }

        Ok(())
    }

    async fn readout(&mut self, input: &mut Input<'_>) -> anyhow::Result<()> {
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

        self.model.modify(|state| init(state.readouts_mut()));

        while !self.model.get(|state| state.readouts().is_ready()) {
            let key = input.get().await?;

            self.model.maybe_modify(|state| {
                let readouts = state.readouts_mut();

                let readout = &mut readouts.readouts[readouts.active].1;

                match key {
                    KeyCode::Enter => {
                        if !readout.is_empty() {
                            readouts.active += 1;
                            return true;
                        }
                    }
                    KeyCode::Esc => {
                        init(readouts);
                        return true;
                    }
                    KeyCode::Backspace => {
                        readout.pop();
                        return true;
                    }
                    KeyCode::Char(ch) => {
                        readout.push(ch);
                        return true;
                    }
                    _ => (),
                }

                false
            });
        }

        Ok(())
    }

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
            input.wait_quit(),
            self.prep_bundle(bundle_id.as_deref()),
        )
        .coalesce()
        .await
    }

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

        // TODO: Support the others too
        let mut zip = ZipArchive::new(File::open(bundle_path)?)?;

        let bundle = Bundle::from_zip_bundle(bundle_name, &mut zip)?;

        self.model
            .modify(move |state| *state = State::Prepared(Prepared { bundle }));

        Ok(())
    }

    async fn load_bundle(&mut self, bundle_id: Option<&str>) -> anyhow::Result<PathBuf> {
        let bundle = loop {
            self.model.modify(|state| {
                state.preparing_mut().status = "Checking".into();
            });

            let loaded_path = self.bundle_dir.join("loaded");
            fs::create_dir_all(&loaded_path)?;

            let files = fs::read_dir(&loaded_path)?
                .map(|e| e.unwrap())
                .collect::<Vec<_>>(); // TODO

            if files.len() > 1 {
                panic!("TODO");
            }

            if files.len() == 1 {
                break files[0].path();
            }

            self.model.modify(|state| {
                state.preparing_mut().status = "Fetching".into();
            });

            let scratch_path = self.bundle_dir.join("scratch").join("bundle");
            fs::create_dir_all(scratch_path.parent().unwrap())?;

            let bundle_name = {
                let mut scratch_file = File::create(&scratch_path)?;

                let result = self.bundle_loader.load(&mut scratch_file, bundle_id).await;

                match result {
                    Ok(bundle_name) => bundle_name,
                    Err(err) => Err(err)?,
                }
            };

            let bundle_path = self.bundle_dir.join("loaded").join(&bundle_name);
            fs::create_dir_all(bundle_path.parent().unwrap())?;

            fs::rename(scratch_path, bundle_path)?;
        };

        Ok(bundle)
    }

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

        let bundle_loaded_dir = self.bundle_dir.join("loaded");
        fs::create_dir_all(&bundle_loaded_dir)?;

        bundle_loaded_dir.read_dir()?.for_each(|entry| {
            // TODO
            let entry = entry.unwrap();
            fs::remove_file(entry.path()).unwrap();
        });

        self.model.modify(|state| {
            *state = State::Provisioned(Provisioned {
                bundle_name: state.provisioning().bundle.name.clone(),
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
