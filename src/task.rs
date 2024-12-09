use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crossterm::event::KeyCode;
use embassy_futures::select::select3;
use embassy_time::{Duration, Ticker};

use espflash::flasher::ProgressCallbacks;
use serde::Deserialize;

use zip::ZipArchive;

use crate::bundle::{Bundle, Efuse, Image, Partition, PartitionFlags, PartitionType};
use crate::flash;
use crate::input::Input;
use crate::loader::BundleLoader;
use crate::model::{Model, Prepared, Preparing, Provisioning, ProvisioningStatus, State};
use crate::utils::futures::{Coalesce, IntoFallibleFuture};

pub struct Task<'a, T> {
    model: Arc<Model>,
    bundle_dir: &'a Path,
    bundle_loader: T,
}

impl<'a, T> Task<'a, T>
where
    T: BundleLoader,
{
    pub fn new(model: Arc<Model>, bundle_dir: &'a Path, bundle_loader: T) -> Self {
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

            break; // TODO
        }

        Ok(())
    }

    async fn prep_bundle_with_ticker(&mut self, input: &mut Input<'_>) -> anyhow::Result<()> {
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

        let mut partitions = Vec::new();

        let part_table_offset = 0x8000; // TODO

        partitions.push(Partition {
            name: "(bootloader)".to_string(),
            part_type: PartitionType::Data, // TODO
            part_subtype: String::new(),
            offset: 0x1000, // TODO: Not for all chips
            size: part_table_offset - 0x1000,
            flags: PartitionFlags::ENCRYPTED, // TODO
            image: None,
        });

        partitions.push(Partition {
            name: "(part-table)".to_string(),
            part_type: PartitionType::Data, // TODO
            part_subtype: String::new(),
            offset: part_table_offset,
            size: 4096,
            flags: PartitionFlags::ENCRYPTED, // TODO
            image: None,
        });

        let mut offset = part_table_offset + 4096;

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
            .filter_map(|name| {
                if name == "bootloader" || name.starts_with("images/") {
                    Some(name.to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        for name in image_names {
            let mut zip_file = zip.by_name(&name).unwrap();

            let mut data = Vec::new();
            zip_file.read_to_end(&mut data).unwrap();

            let data = Image {
                data: Arc::new(data),
                status: ProvisioningStatus::NotStarted,
            };

            let partition = partitions.iter_mut().find(|partition| {
                partition.name == name
                    || Some(partition.name.as_ref()) == name.strip_prefix("images/")
            });

            if let Some(partition) = partition {
                partition.image = Some(data);
            }
        }

        let efuse_names = zip
            .file_names()
            .filter_map(|name| name.starts_with("efuse/").then_some(name.to_string()))
            .collect::<Vec<_>>();

        let efuses = efuse_names
            .into_iter()
            .map(|name| {
                let mut zip_file = zip.by_name(name.as_str()).unwrap();

                let mut data = Vec::new();
                zip_file.read_to_end(&mut data).unwrap();

                Efuse {
                    name: name.strip_prefix("efuse/").unwrap().to_string(),
                    file_name: name,
                    data: Arc::new(data),
                }
            })
            .collect::<Vec<_>>();

        self.model.modify(|state| {
            *state = State::Prepared(Prepared {
                bundle: Bundle {
                    name: bundle_name,
                    partitions,
                    efuses,
                },
            })
        });

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

            for partition in &mut ps.bundle.partitions {
                if let Some(image) = partition.image.as_mut() {
                    image.status = ProvisioningStatus::Pending;
                }
            }
        });

        let images = self.model.get(|state| {
            let ps = state.provisioning();

            ps.bundle
                .partitions
                .iter()
                .filter_map(|partition| {
                    partition
                        .image
                        .as_ref()
                        .map(|image| (image.data.clone(), partition.offset))
                })
                .collect::<Vec<_>>()
        });

        flash::flash(
            "/dev/ttyUSB0",
            images,
            FlashProgress::new(self.model.clone()),
        )
        .await?;

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
            part_subtype: self.part_subtype.trim().to_string(),
            offset: Self::offset(&self.offset).unwrap_or(offset),
            size: Self::size(&self.size),
            flags: PartitionFlags::ENCRYPTED, //self.flags,
            image: None,
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

struct FlashProgress {
    model: Arc<Model>,
    image: Mutex<Option<(String, usize)>>,
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
        self.model.get(|state| {
            let ps = state.provisioning();

            if let Some(partition) = ps
                .bundle
                .partitions
                .iter()
                .find(|partition| partition.offset == addr as usize)
            {
                *self.image.lock().unwrap() = Some((partition.name.clone(), total));
            }
        });
    }

    fn update(&mut self, current: usize) {
        if let Some(image) = self.image.lock().unwrap().as_ref() {
            self.model.modify(|state| {
                let ps = state.provisioning_mut();

                let partition = ps
                    .bundle
                    .partitions
                    .iter_mut()
                    .find(|partition| partition.name == image.0);

                if let Some(partition) = partition {
                    if let Some(imaged) = partition.image.as_mut() {
                        imaged.status =
                            ProvisioningStatus::InProgress((current * 100 / image.1) as u8);
                    }
                }
            });
        }
    }

    fn finish(&mut self) {
        if let Some(image) = self.image.lock().unwrap().as_ref() {
            self.model.modify(|state| {
                let ps = state.provisioning_mut();

                let partition = ps
                    .bundle
                    .partitions
                    .iter_mut()
                    .find(|partition| partition.name == image.0);

                if let Some(partition) = partition {
                    if let Some(imaged) = partition.image.as_mut() {
                        imaged.status = ProvisioningStatus::Done;
                    }
                }
            });
        }

        *self.image.lock().unwrap() = None;
    }
}
