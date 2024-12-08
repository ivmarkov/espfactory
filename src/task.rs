use std::fs::{self, File};
use std::path::{Path, PathBuf};

use crossterm::event::KeyCode;
use embassy_futures::select::{select, select3};
use embassy_time::{Duration, Ticker};

use serde::Deserialize;

use zip::ZipArchive;

use crate::bundle::{Bundle, Efuse, Image, Partition, PartitionFlags, PartitionType};
use crate::input::Input;
use crate::loader::BundleLoader;
use crate::model::{Model, Prepared, Preparing, Provisioning, State};
use crate::utils::futures::{Coalesce, IntoFallibleFuture};

pub struct Task<'a, T> {
    model: &'a Model,
    bundle_dir: &'a Path,
    bundle_loader: T,
}

impl<'a, T> Task<'a, T>
where
    T: BundleLoader,
{
    pub const fn new(model: &'a Model, bundle_dir: &'a Path, bundle_loader: T) -> Self {
        Self {
            model,
            bundle_dir,
            bundle_loader,
        }
    }

    pub async fn run(&mut self, input: &mut Input<'_>) -> anyhow::Result<()> {
        loop {
            self.model.modify(|state| *state = State::new());

            self.prep_bundle_with_ticker(input).await?;

            if !input.wait_quit_or(KeyCode::Enter).await? {
                break;
            }

            self.provision().await?;
        }

        Ok(())
    }

    async fn prep_bundle_with_ticker(&mut self, input: &mut Input<'_>) -> anyhow::Result<()> {
        select3(
            Self::tick(Duration::from_millis(100), || {
                self.model.modify(|state| {
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
            state.preparing().status = format!("Processing bundle {bundle_name}");
        });

        let mut zip = ZipArchive::new(File::open(bundle_path)?)?;

        let mut partitions = Vec::new();
        let mut offset = 0;

        for rp in csv::ReaderBuilder::new()
            .has_headers(false)
            .delimiter(b',')
            .double_quote(false)
            .escape(Some(b'\\'))
            .flexible(true)
            .comment(Some(b'#'))
            .from_reader(zip.by_name("partition-table.csv")?)
            .deserialize::<UnprocessedPartition>()
        {
            let rp = rp?;

            let partition = rp.process(offset)?;
            offset = partition.offset + partition.size;

            partitions.push(partition);
        }

        let image_names = zip
            .file_names()
            .filter_map(|name| name.starts_with("images/").then_some(name.to_string()))
            .collect::<Vec<_>>();

        let images = image_names
            .into_iter()
            .map(|name| {
                let zip_file = zip.by_name(name.as_str()).unwrap();

                Image {
                    name: name.strip_prefix("images/").unwrap().to_string(),
                    file_name: name,
                    size: zip_file.size() as usize,
                }
            })
            .collect::<Vec<_>>();

        let efuse_names = zip
            .file_names()
            .filter_map(|name| name.starts_with("efuse/").then_some(name.to_string()))
            .collect::<Vec<_>>();

        let efuses = efuse_names
            .into_iter()
            .map(|name| {
                let zip_file = zip.by_name(name.as_str()).unwrap();

                Efuse {
                    name: name.strip_prefix("efuse/").unwrap().to_string(),
                    file_name: name,
                    size: zip_file.size() as usize,
                }
            })
            .collect::<Vec<_>>();

        self.model.modify(|state| {
            *state = State::Prepared(Prepared {
                bundle: Bundle {
                    name: bundle_name,
                    bootloader: None, // TODO
                    partitions,
                    images,
                    efuses,
                },
            })
        });

        Ok(())
    }

    async fn load_bundle(&mut self) -> anyhow::Result<PathBuf> {
        let bundle = loop {
            self.model.modify(|state| {
                state.preparing().status = "Checking".into();
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
                state.preparing().status = "Fetching".into();
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
                bundle: state.prepared().bundle.clone(),
                images_status: Default::default(),
                efuses_status: Default::default(),
            })
        });

        // espflash::flasher::Flasher::connect(serial, port_info, speed, use_stub, verify, skip, chip, after_operation, before_operation)
        core::future::pending().await
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

#[derive(Debug, Clone, Deserialize)]
pub struct UnprocessedPartition {
    pub name: String,
    pub part_type: String, //PartitionType,
    pub part_subtype: String,
    pub offset: String,
    pub size: String,
    pub flags: String,
}

impl UnprocessedPartition {
    fn process(&self, offset: usize) -> anyhow::Result<Partition> {
        // TODO

        Ok(Partition {
            name: self.name.clone(),
            part_type: PartitionType::App, // self.part_type.parse()?,
            part_subtype: self.part_subtype.clone(),
            offset: Self::offset(&self.offset).unwrap_or(offset),
            size: Self::size(&self.size),
            flags: PartitionFlags::ENCRYPTED, //self.flags,
        })
    }

    fn offset(offset: &str) -> Option<usize> {
        let offset = offset.trim().to_ascii_lowercase();

        if offset.is_empty() {
            None
        } else if let Some(offset) = offset.strip_prefix("0x") {
            Some(usize::from_str_radix(offset, 16).unwrap()) // TODO
        } else {
            Some(offset.parse().unwrap())
        }
    }

    fn size(size: &str) -> usize {
        let size = size.trim().to_ascii_lowercase();

        if let Some(size) = size.strip_suffix("k") {
            size.parse::<usize>().unwrap() * 1024
        } else if let Some(size) = size.strip_suffix("m") {
            size.parse::<usize>().unwrap() * 1024 * 1024
        } else if let Some(size) = size.strip_prefix("0x") {
            usize::from_str_radix(size, 16).unwrap() // TODO
        } else {
            size.parse().unwrap()
        }
    }
}
