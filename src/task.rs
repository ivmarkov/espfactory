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
use crate::flash;
use crate::input::Input;
use crate::loader::BundleLoader;
use crate::model::{Model, Prepared, Preparing, Provisioned, Provisioning, State};
use crate::utils::futures::{Coalesce, IntoFallibleFuture};

extern crate alloc;

pub struct Task<'a, T> {
    model: Arc<Model>,
    com_port: Option<&'a str>,
    bundle_dir: &'a Path,
    bundle_loader: T,
}

impl<'a, T> Task<'a, T>
where
    T: BundleLoader,
{
    pub fn new(
        model: Arc<Model>,
        com_port: Option<&'a str>,
        bundle_dir: &'a Path,
        bundle_loader: T,
    ) -> Self {
        Self {
            model,
            com_port,
            bundle_dir,
            bundle_loader,
        }
    }

    pub async fn run(&mut self, input: &mut Input<'_>) -> anyhow::Result<()> {
        loop {
            self.model.modify(|state| *state = State::new());

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

    async fn prepare(&mut self, input: &mut Input<'_>) -> anyhow::Result<()> {
        let model = self.model.clone();

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
            self.prep_bundle(),
        )
        .coalesce()
        .await
    }

    async fn prep_bundle(&mut self) -> anyhow::Result<()> {
        let bundle_path = self.load_bundle().await?;

        let bundle_name = bundle_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string(); // TODO

        self.model.modify(|state| {
            state.preparing_mut().status = format!("Processing bundle {bundle_name}");
        });

        let mut zip = ZipArchive::new(File::open(bundle_path)?)?;

        let bundle = Bundle::from_zip_bundle(bundle_name, &mut zip)?;

        self.model
            .modify(move |state| *state = State::Prepared(Prepared { bundle }));

        Ok(())
    }

    async fn load_bundle(&mut self) -> anyhow::Result<PathBuf> {
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

                let result = self.bundle_loader.load(&mut scratch_file).await;

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
            self.com_port.unwrap_or("/dev/ttyUSB0"), // TODO
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
