use core::iter::once;

use std::collections::HashMap;
use std::io::{Read, Seek};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use esp_idf_part::{Partition, SubType, Type};

use espflash::flasher::FlashSize;
use log::warn;
use serde::Deserialize;

use zip::ZipArchive;

use crate::flash;

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

    pub const PAGE_SIZE: usize = 4096;

    const PARAMS_FILE_NAME: &str = "params.toml";
    const BOOTLOADER_FILE_NAME: &str = "bootloader.bin";
    const PART_TABLE_FILE_NAME: &str = "partition-table.csv";

    const BIN_SUFFIX: &str = ".bin";

    const IMAGES_PREFIX: &str = "images/";
    const EFUSES_PREFIX: &str = "efuse/";

    const PART_TABLE_SIZE: usize = Self::PAGE_SIZE;

    // (4MB flash size assumed)
    //
    // Table is optimized so that it can hold a larger (signed) bootloader
    // as well as two signed app images, whose partitions' start (as always) is
    // aligned to 64K, and whose size (excluding the potential 4K signature at
    // the end) is divisible by 64K
    const DEFAULT_PART_TABLE: &str = r#"
# Name,   Type, SubType,   Offset,  Size,  Flags
ota_0,    app,  ota_0,    0x10000, 1956K,
nvs,      data, nvs,             ,   32K,
nvs_keys, data, 0x04,            ,    4K,
phy_init, data, phy,             ,    4K,
extra_0,  data, 0x06,            ,   20K,  
ota_1,    app,  ota_1,           , 1956K,
nvs_bm,   data, 0x06,            ,   32K,
otadata,  data, ota,             ,    8K,
extra_1,  data, 0x06,            ,   20K,
"#;

    pub fn from_elf_app_image(
        name: String,
        params: Params,
        app_image: &[u8],
    ) -> anyhow::Result<Self> {
        let app_image = Image::new_elf(flash::elf2bin(app_image, params.chip)?);

        Self::from_parts(
            name,
            params,
            None,
            None,
            once(("ota_1".to_string(), app_image)),
            Vec::new().into_iter(),
        )
    }

    pub fn from_bin_app_image(
        name: String,
        params: Params,
        app_image: &[u8],
    ) -> anyhow::Result<Self> {
        let app_image = Image::new(app_image.to_vec());

        Self::from_parts(
            name,
            params,
            None,
            None,
            once(("ota_1".to_string(), app_image)),
            Vec::new().into_iter(),
        )
    }

    pub fn from_zip_bundle<T>(name: String, zip: &mut ZipArchive<T>) -> anyhow::Result<Self>
    where
        T: Read + Seek,
    {
        let mut params_str = String::new();
        zip.by_name(Self::PARAMS_FILE_NAME)?
            .read_to_string(&mut params_str)?;

        let params: Params = toml::from_str(&params_str)?;

        let part_table_str = zip
            .index_for_name(Self::PART_TABLE_FILE_NAME)
            .map(|index| {
                let mut zip_file = zip.by_index(index)?;
                let mut part_table_str = String::new();

                zip_file.read_to_string(&mut part_table_str)?;

                Ok::<_, anyhow::Error>(part_table_str)
            })
            .transpose()?;

        let bootloader_image = zip
            .index_for_name(Self::BOOTLOADER_FILE_NAME)
            .map(|index| {
                let mut zip_file = zip.by_index(index)?;

                let mut data = Vec::new();
                zip_file.read_to_end(&mut data)?;

                Ok::<_, anyhow::Error>(Image::new(data))
            })
            .transpose()?;

        let image_names = zip
            .file_names()
            .filter(|file_name| file_name.starts_with(Self::IMAGES_PREFIX))
            .map(|file_name| file_name.to_string())
            .collect::<Vec<_>>();

        let images: anyhow::Result<Vec<_>> = image_names
            .into_iter()
            .map(|file_name| {
                let mut zip_file = zip.by_name(&file_name)?;

                let mut data = Vec::new();
                zip_file.read_to_end(&mut data)?;

                let name = name
                    .strip_prefix(Self::IMAGES_PREFIX)
                    .unwrap()
                    .trim_end_matches(Self::BIN_SUFFIX);

                let elf = file_name.ends_with(Self::BIN_SUFFIX);

                let image = if elf {
                    Image::new(flash::elf2bin(&data, params.chip)?)
                } else {
                    Image::new(data)
                };

                Ok((name.to_string(), image))
            })
            .collect();

        let images = images?;

        let efuse_names = zip
            .file_names()
            .filter(|file_name| file_name.starts_with(Self::EFUSES_PREFIX))
            .map(|file_name| file_name.to_string())
            .collect::<Vec<_>>();

        let efuses: anyhow::Result<Vec<_>> = efuse_names
            .into_iter()
            .map(|file_name| {
                let mut zip_file = zip.by_name(file_name.as_str())?;

                let mut data = Vec::new();
                zip_file.read_to_end(&mut data)?;

                Ok(Efuse {
                    name: file_name
                        .strip_prefix(Self::EFUSES_PREFIX)
                        .unwrap()
                        .to_string(),
                    data: Arc::new(data),
                })
            })
            .collect();

        let efuses = efuses?;

        Self::from_parts(
            name,
            params,
            part_table_str.as_deref(),
            bootloader_image,
            images.into_iter(),
            efuses.into_iter(),
        )
    }

    fn from_parts(
        name: String,
        params: Params,
        part_table_str: Option<&str>,
        bootloader: Option<Image>,
        images: impl Iterator<Item = (String, Image)>,
        efuses: impl Iterator<Item = Efuse>,
    ) -> anyhow::Result<Self> {
        let images = images.collect::<HashMap<_, _>>();

        let part_table_str = part_table_str.unwrap_or(Self::DEFAULT_PART_TABLE);
        let bootloader = Ok(bootloader)
            .transpose()
            .unwrap_or_else(|| flash::default_bootloader(params.chip).map(Image::new))?;

        let part_table = esp_idf_part::PartitionTable::try_from_str(part_table_str).unwrap();
        let part_table_image = Image::new(part_table.to_bin().unwrap());

        let part_table_offset = part_table.partitions()[0].offset() - Self::PART_TABLE_SIZE as u32;

        let bootloader_offset = params.chip.boot_addr();
        let bootloader_size = part_table_offset - bootloader_offset;

        let parts_mapping: anyhow::Result<Vec<PartitionMapping>> = once(Ok(PartitionMapping {
            partition: Partition::new(
                Self::BOOTLOADER_NAME,
                Type::Custom(0),
                SubType::Custom(0),
                bootloader_offset,
                bootloader_size,
                false,
            ),
            image: Some(bootloader),
        }))
        .chain(once(Ok(PartitionMapping {
            partition: Partition::new(
                Self::PART_TABLE_NAME,
                Type::Custom(0),
                SubType::Custom(0),
                part_table_offset,
                Self::PART_TABLE_SIZE as _,
                false,
            ),
            image: Some(part_table_image),
        })))
        .chain(
            part_table
                .partitions()
                .iter()
                .map(|partition| {
                    let image = images.get(partition.name().as_str());
                    if let Some(image) = image {
                        if image.elf {
                            if matches!(partition.ty(), Type::App) {
                                warn!("ELF image found for partition '{}', prefer `.bin` files, as they take less space", partition.name());
                            } else {
                                anyhow::bail!("Partition '{}' is not of type 'App', but an ELF image was provided", partition.name());
                            }
                        }
                    }

                    Ok(PartitionMapping {
                        partition: partition.clone(),
                        image: image.cloned(),
                    })
                }),
        )
        .collect();

        Ok(Self {
            name,
            params,
            parts_mapping: parts_mapping?,
            efuse_mapping: efuses.collect(),
        })
    }

    pub fn get_flash_data(&self) -> impl Iterator<Item = FlashData> + '_ {
        self.parts_mapping.iter().filter_map(|mapping| {
            mapping.image.as_ref().map(|image| FlashData {
                offset: mapping.partition.offset(),
                data: image.data.clone(),
            })
        })
    }

    pub fn set_status_all(&mut self, status: ProvisioningStatus) -> bool {
        let mut modified = false;

        for mapping in &mut self.parts_mapping {
            if let Some(image) = mapping.image.as_mut() {
                if image.status != status {
                    image.status = status;
                    modified = true;
                }
            }
        }

        modified
    }

    pub fn set_status(&mut self, part_offset: u32, status: ProvisioningStatus) -> bool {
        let mut modified = false;

        for mapping in &mut self.parts_mapping {
            if mapping.partition.offset() == part_offset {
                if let Some(image) = mapping.image.as_mut() {
                    if image.status != status {
                        image.status = status;
                        modified = true;
                    }
                }
            }
        }

        modified
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Params {
    /// Chip type to be flashed
    pub chip: Chip,
    /// Flash size of the target device
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
        match self {
            Self::Esp32 | Self::Esp32s2 => 0x1000,
            Self::Esp32p4 => 0x2000,
            _ => 0x0,
        }
    }

    pub fn to_flash_chip(self) -> espflash::targets::Chip {
        match self {
            Self::Esp32 => espflash::targets::Chip::Esp32,
            Self::Esp32c2 => espflash::targets::Chip::Esp32c2,
            Self::Esp32c3 => espflash::targets::Chip::Esp32c3,
            Self::Esp32c6 => espflash::targets::Chip::Esp32c6,
            Self::Esp32h2 => espflash::targets::Chip::Esp32h2,
            Self::Esp32p4 => espflash::targets::Chip::Esp32p4,
            Self::Esp32s2 => espflash::targets::Chip::Esp32s2,
            Self::Esp32s3 => espflash::targets::Chip::Esp32s3,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FlashData {
    pub offset: u32,
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
    pub elf: bool,
    pub status: ProvisioningStatus,
}

impl Image {
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            data: Arc::new(data),
            elf: false,
            status: ProvisioningStatus::NotStarted,
        }
    }

    pub fn new_elf(data: Vec<u8>) -> Self {
        Self {
            data: Arc::new(data),
            elf: true,
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
