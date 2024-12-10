use core::iter::once;

use std::collections::HashMap;
use std::io::{Read, Seek};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use esp_idf_part::{Partition, SubType, Type};

use espflash::flasher::FlashSize;
use serde::Deserialize;

use zip::ZipArchive;

extern crate alloc;

#[derive(Clone, Debug)]
pub struct Bundle {
    pub name: String,
    pub params: Params,
    pub parts_mapping: Vec<PartitionMapping>,
    pub efuse_mapping: Vec<Efuse>,
}

impl Bundle {
    pub const BOOTLOADER_NAME: &str = "(bootloader)";
    pub const PART_TABLE_NAME: &str = "(part-table)";

    const PARAMS_FILE_NAME: &str = "params.toml";
    const BOOTLOADER_FILE_NAME: &str = "bootloader";
    const PART_TABLE_FILE_NAME: &str = "partition-table.csv";

    const IMAGES_PREFIX: &str = "images/";
    const EFUSES_PREFIX: &str = "efuse/";

    const PART_TABLE_SIZE: usize = 4096;

    pub fn create<T>(name: String, zip: &mut ZipArchive<T>) -> anyhow::Result<Self>
    where
        T: Read + Seek,
    {
        let mut params_str = String::new();
        zip.by_name(Self::PARAMS_FILE_NAME)?
            .read_to_string(&mut params_str)?;

        let params: Params = toml::from_str(&params_str)?;

        let mut part_table_str = String::new();
        zip.by_name(Self::PART_TABLE_FILE_NAME)?
            .read_to_string(&mut part_table_str)?;

        let bootloader_image =
            if let Some(bootloader_index) = zip.index_for_name(Self::BOOTLOADER_FILE_NAME) {
                let mut zip_file = zip.by_index(bootloader_index).unwrap();

                let mut data = Vec::new();
                zip_file.read_to_end(&mut data).unwrap();

                Some(Image::new(data))
            } else {
                None
            };

        let part_table = esp_idf_part::PartitionTable::try_from_str(&part_table_str).unwrap();
        let part_table_image = Image::new(part_table.to_bin().unwrap());

        let part_table_offset = part_table.partitions()[0].offset() - Self::PART_TABLE_SIZE as u32;

        let bootloader_offset = params.chip.boot_addr();
        let bootloader_size = part_table_offset - bootloader_offset;

        let image_names = zip
            .file_names()
            .filter(|name| name.starts_with(Self::IMAGES_PREFIX))
            .map(|name| name.to_string())
            .collect::<Vec<_>>();

        let images = image_names
            .into_iter()
            .map(|name| {
                let mut zip_file = zip.by_name(&name).unwrap();

                let mut data = Vec::new();
                zip_file.read_to_end(&mut data).unwrap();

                (
                    name.strip_prefix(Self::IMAGES_PREFIX).unwrap().to_string(),
                    Image::new(data),
                )
            })
            .collect::<HashMap<_, _>>();

        let efuse_names = zip
            .file_names()
            .filter(|name| name.starts_with(Self::EFUSES_PREFIX))
            .map(|name| name.to_string())
            .collect::<Vec<_>>();

        let efuse_mapping = efuse_names
            .into_iter()
            .map(|name| {
                let mut zip_file = zip.by_name(name.as_str()).unwrap();

                let mut data = Vec::new();
                zip_file.read_to_end(&mut data).unwrap();

                Efuse {
                    name: name.strip_prefix(Self::EFUSES_PREFIX).unwrap().to_string(),
                    data: Arc::new(data),
                }
            })
            .collect::<Vec<_>>();

        let parts_mapping = once(PartitionMapping {
            partition: Partition::new(
                Self::BOOTLOADER_NAME,
                Type::Custom(0),
                SubType::Custom(0),
                bootloader_offset,
                bootloader_size,
                false,
            ),
            image: bootloader_image,
        })
        .chain(once(PartitionMapping {
            partition: Partition::new(
                Self::PART_TABLE_NAME,
                Type::Custom(0),
                SubType::Custom(0),
                part_table_offset,
                Self::PART_TABLE_SIZE as _,
                false,
            ),
            image: Some(part_table_image),
        }))
        .chain(
            part_table
                .partitions()
                .iter()
                .map(|partition| PartitionMapping {
                    partition: partition.clone(),
                    image: images.get(partition.name().as_str()).cloned(),
                }),
        )
        .collect::<Vec<_>>();

        Ok(Self {
            name,
            params,
            parts_mapping,
            efuse_mapping,
        })
    }

    pub fn get_flash_data(&self) -> impl Iterator<Item = FlashData> + '_ {
        self.parts_mapping.iter().filter_map(|mapping| {
            mapping.image.as_ref().map(|image| FlashData {
                offsert: mapping.partition.offset(),
                data: image.data.clone(),
            })
        })
    }

    pub fn set_status_all(&mut self, status: ProvisioningStatus) {
        for mapping in &mut self.parts_mapping {
            if let Some(image) = mapping.image.as_mut() {
                image.status = status;
            }
        }
    }

    pub fn set_status(&mut self, part_offset: u32, status: ProvisioningStatus) {
        for mapping in &mut self.parts_mapping {
            if mapping.partition.offset() == part_offset {
                if let Some(image) = mapping.image.as_mut() {
                    image.status = status;
                }
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Params {
    pub chip: Chip,
    pub flash_size: Option<FlashSize>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Deserialize)]
#[non_exhaustive]
//#[strum(serialize_all = "lowercase")]
pub enum Chip {
    /// ESP32
    Esp32,
    /// ESP32-C2, ESP8684
    Esp32c2,
    /// ESP32-C3, ESP8685
    Esp32c3,
    /// ESP32-C6
    Esp32c6,
    /// ESP32-H2
    Esp32h2,
    /// ESP32-P4
    Esp32p4,
    /// ESP32-S2
    Esp32s2,
    /// ESP32-S3
    Esp32s3,
}

impl Chip {
    pub const fn boot_addr(&self) -> u32 {
        0 // TODO
    }

    pub fn to_flash_chip(self) -> espflash::targets::Chip {
        match self {
            Chip::Esp32 => espflash::targets::Chip::Esp32,
            Chip::Esp32c2 => espflash::targets::Chip::Esp32c2,
            Chip::Esp32c3 => espflash::targets::Chip::Esp32c3,
            Chip::Esp32c6 => espflash::targets::Chip::Esp32c6,
            Chip::Esp32h2 => espflash::targets::Chip::Esp32h2,
            Chip::Esp32p4 => espflash::targets::Chip::Esp32p4,
            Chip::Esp32s2 => espflash::targets::Chip::Esp32s2,
            Chip::Esp32s3 => espflash::targets::Chip::Esp32s3,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FlashData {
    pub offsert: u32,
    pub data: Arc<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct PartitionMapping {
    pub partition: Partition,
    pub image: Option<Image>,
}

impl PartitionMapping {
    pub fn status(&self) -> Option<ProvisioningStatus> {
        self.image.as_ref().map(|image| image.status)
    }
}

#[derive(Clone, Debug)]
pub struct Efuse {
    pub name: String,
    pub data: Arc<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct Image {
    pub data: Arc<Vec<u8>>,
    pub status: ProvisioningStatus,
}

impl Image {
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            data: Arc::new(data),
            status: ProvisioningStatus::NotStarted,
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum ProvisioningStatus {
    NotStarted,
    Pending,
    InProgress(u8),
    Done,
}
