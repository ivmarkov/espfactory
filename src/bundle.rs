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
use crate::loader::BundleType;

extern crate alloc;

/// Represents a loaded bundle ready for flashing and e-fuse programming
#[derive(Clone, Debug)]
pub struct Bundle {
    /// The name of the bundle. Used for display purposes only
    pub name: String,
    /// The parameters of the bundle (chip and an optional flash size)
    pub params: Params,
    /// The mapping of partitions to images
    pub parts_mapping: Vec<PartitionMapping>,
    /// The mapping of efuses to efuse regions (TBD)
    pub efuse_mapping: Vec<Efuse>,
}

impl Bundle {
    /// The name of the bootloader pseudo-partition
    /// This partition is not really present in the partition table, but is used for rendering purposes
    pub const BOOTLOADER_NAME: &str = "(bootloader)";
    /// The name of the partition table pseudo-partition
    /// This partition is not really present in the partition table, but is used for rendering purposes
    pub const PART_TABLE_NAME: &str = "(part-table)";

    /// The size of a flash page in Espressif chips
    pub const PAGE_SIZE: usize = 4096;

    /// The name of the parameters file when loaded from a ZIP bundle (.bundle)
    const PARAMS_FILE_NAME: &str = "params.toml";
    /// The name of the bootloader file when loaded from a ZIP bundle (.bundle)
    const BOOTLOADER_FILE_NAME: &str = "bootloader.bin";
    /// The name of the partition table file when loaded from a ZIP bundle (.bundle)
    const PART_TABLE_FILE_NAME: &str = "partition-table.csv";

    /// The suffix of the binary image files when loaded from a ZIP bundle (.bundle)
    const BIN_SUFFIX: &str = ".bin";

    /// The prefix of the image files when loaded from a ZIP bundle (.bundle)
    const IMAGES_PREFIX: &str = "images/";
    /// The prefix of the efuse files when loaded from a ZIP bundle (.bundle)
    const EFUSES_PREFIX: &str = "efuse/";

    /// The size of the partition table in Espressif chips
    const PART_TABLE_SIZE: usize = Self::PAGE_SIZE;

    /// The default partition table which is used when the partition table is not provided in the bundle
    /// (i.e. ZIP bundles with no `partition-table.csv` file as well as binary and ELF app images)
    ///
    /// (4MB flash size assumed)
    ///
    /// Table is optimized so that it can hold a larger (signed) bootloader
    /// as well as two signed app images, whose partitions' start (as always) is
    /// aligned to 64K, and whose size (excluding the potential 4K signature at
    /// the end) is divisible by 64K
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

    /// Create a new `Bundle` from a bundle content
    ///
    /// # Arguments
    /// - `name`: The name of the bundle
    ///   Used to identify the type of the provided `bundle_content` by examining the suffix in the name as
    ///   well as for display purposes
    /// - `default_params`: The default parameters to use when the parameters are not provided in the bundle
    /// - `bundle_content`: The content of the bundle (a ZIP archive, a binary image, or an ELF image)
    pub fn create<R>(
        name: String,
        default_params: Params,
        mut bundle_content: R,
    ) -> anyhow::Result<Self>
    where
        R: Read + Seek,
    {
        let bundle_type = BundleType::iter()
            .find(|&bundle_type| name.ends_with(bundle_type.suffix()))
            .ok_or_else(|| {
                anyhow::anyhow!("Bundle name '{}' does not end with a known suffix", name)
            })?;

        match bundle_type {
            BundleType::Complete => {
                Self::from_zip_bundle(name, &mut ZipArchive::new(bundle_content)?)
            }
            BundleType::BinAppImage => {
                let mut bytes = Vec::new();
                bundle_content.read_to_end(&mut bytes)?;

                Self::from_bin_app_image(name, default_params, &bytes)
            }
            BundleType::ElfAppImage => {
                let mut bytes = Vec::new();
                bundle_content.read_to_end(&mut bytes)?;

                Self::from_elf_app_image(name, default_params, &bytes)
            }
        }
    }

    /// Create a new `Bundle` from an ELF application image
    ///
    /// # Arguments
    /// - `name`: The name of the bundle
    /// - `params`: The parameters of the bundle (chip and an optional flash size)
    /// - `app_image`: The content of the ELF application image
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

    /// Create a new `Bundle` from a binary application image
    ///
    /// # Arguments
    /// - `name`: The name of the bundle
    /// - `params`: The parameters of the bundle (chip and an optional flash size)
    /// - `app_image`: The content of the binary application image
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

    /// Create a new `Bundle` from a ZIP bundle
    ///
    /// # Arguments
    /// - `name`: The name of the bundle
    /// - `zip`: The ZIP archive containing the bundle content
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

    /// Create a new `Bundle` from the parts of the bundle
    ///
    /// # Arguments
    /// - `name`: The name of the bundle
    /// - `params`: The parameters of the bundle (chip and an optional flash size)
    /// - `part_table_str`: The partition table as a string; if `None`, the default partition table is used
    /// - `bootloader`: The bootloader image; if `None`, a default bootloader is used
    /// - `images`: The images to be flashed to the partitions, where the key is the partition name
    /// - `efuses`: The efuses to be programmed, where the key is the efuse name (TBD)
    pub fn from_parts(
        name: String,
        params: Params,
        part_table_str: Option<&str>,
        bootloader: Option<Image>,
        images: impl Iterator<Item = (String, Image)>,
        efuses: impl Iterator<Item = Efuse>,
    ) -> anyhow::Result<Self> {
        let images = images.collect::<HashMap<_, _>>();

        let part_table_str = part_table_str.unwrap_or(Self::DEFAULT_PART_TABLE);
        let bootloader = Ok(bootloader).transpose().unwrap_or_else(|| {
            flash::default_bootloader(params.chip, params.flash_size).map(Image::new)
        })?;

        let part_table = esp_idf_part::PartitionTable::try_from_str(part_table_str)?;
        let part_table_image = Image::new(part_table.to_bin()?);

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

    /// Get the flash data to be flashed to the device
    pub(crate) fn get_flash_data(&self) -> impl Iterator<Item = FlashData> + '_ {
        self.parts_mapping.iter().filter_map(|mapping| {
            mapping.image.as_ref().map(|image| FlashData {
                offset: mapping.partition.offset(),
                data: image.data.clone(),
            })
        })
    }

    /// Set the status of all images to the given status
    pub(crate) fn set_status_all(&mut self, status: ProvisioningStatus) -> bool {
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

    /// Set the status of the image for the given partition to the given status
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

/// The parameters of the bundle
#[derive(Clone, Debug, Deserialize)]
pub struct Params {
    /// Chip type to be flashed
    pub chip: Chip,
    /// Flash size of the target device
    /// If not provided, 4MB flash size is assumed
    pub flash_size: Option<FlashSize>,
}

impl Params {
    /// Create a new `Params` with default values (ESP32 chip and no flash specific size, i.e. 4MB)
    pub const fn new() -> Self {
        Self {
            chip: Chip::Esp32,
            flash_size: None,
        }
    }
}

impl Default for Params {
    fn default() -> Self {
        Self::new()
    }
}

/// The type of the chip to be flashed
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Deserialize)]
#[non_exhaustive]
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
    /// Get the boot address of the chip
    pub const fn boot_addr(&self) -> u32 {
        match self {
            Self::Esp32 | Self::Esp32s2 => 0x1000,
            Self::Esp32p4 => 0x2000,
            _ => 0x0,
        }
    }

    /// Convert the `Chip` to a `espflash::targets::Chip` instance
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

/// The data to be flashed to the device
#[derive(Clone, Debug)]
pub struct FlashData {
    /// The offset in the flash memory
    pub offset: u32,
    /// The data to be flashed
    pub data: Arc<Vec<u8>>,
}

/// The mapping of a partition to an image
///
/// Such a mapping is created for each partition in the partition table, as well as for the bootloader and the partition table itself
/// If there is no image for a partition, the image is `None`
#[derive(Clone, Debug)]
pub struct PartitionMapping {
    /// The partition
    pub partition: Partition,
    /// The image to be flashed to the partition; if `None`, the partition will be left empty
    pub image: Option<Image>,
}

impl PartitionMapping {
    /// Get the status of the image
    pub fn status(&self) -> Option<ProvisioningStatus> {
        self.image.as_ref().map(|image| image.status)
    }
}

/// The mapping of an efuse to an efuse region
/// TBD
#[derive(Clone, Debug)]
pub struct Efuse {
    pub name: String,
    pub data: Arc<Vec<u8>>,
}

/// An image to be flashed to some partition
#[derive(Debug, Clone)]
pub struct Image {
    /// The data of the image
    pub data: Arc<Vec<u8>>,
    /// Was the image originally provided as an ELF file
    /// Only necessary to know for some sanity checks done during bundle loading,
    /// as in trying to associate an ELF image to a non-App partition
    pub elf: bool,
    /// The status of the image flashing
    pub status: ProvisioningStatus,
}

impl Image {
    /// Create a new `Image` from the given binary data, where the binary data
    /// was not extracted from an ELF file
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            data: Arc::new(data),
            elf: false,
            status: ProvisioningStatus::NotStarted,
        }
    }

    /// Create a new `Image` from the given binary data, where the binary data
    /// was extracted from an ELF file
    pub fn new_elf(data: Vec<u8>) -> Self {
        Self {
            data: Arc::new(data),
            elf: true,
            status: ProvisioningStatus::NotStarted,
        }
    }
}

/// The status of the provisioning process for a particular partition + image
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum ProvisioningStatus {
    /// The provisioning process has not started yet
    NotStarted,
    /// The provisioning process is pending
    Pending,
    /// The provisioning process is in progress
    InProgress(u8),
    /// The provisioning process has been completed
    Done,
}
