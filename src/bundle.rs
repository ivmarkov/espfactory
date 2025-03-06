use core::fmt::{self, Display};
use core::iter::once;

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::io::{Read, Seek};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use anyhow::Context;
use esp_idf_part::{Partition, PartitionTable, SubType, Type};

use espflash::flasher::FlashSize;
use log::{info, warn};
use serde::Deserialize;

use zip::ZipArchive;

use crate::flash::{self, empty_space};
use crate::loader::BundleType;

extern crate alloc;

/// Represents a loaded bundle ready for flashing and e-fuse programming
#[derive(Clone, Debug)]
pub struct Bundle {
    /// The name of the bundle.
    pub name: String,
    /// The parameters of the bundle (chip and an optional flash size)
    pub params: Params,
    /// The mapping of partitions to images
    pub parts_mapping: Vec<PartitionMapping>,
    /// The mapping of efuses to efuse regions
    pub efuse_mapping: Vec<EfuseMapping>,
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
    const EFUSES_PREFIX: &str = "efuses/";

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
    /// - `supply_default_part_table`: Whether to supply the default partition table if the partition table is not provided in the bundle
    /// - `supply_default_bootloader`: Whether to supply the default bootloader if the bootloader is not provided in the bundle
    pub fn create<R>(
        name: String,
        default_params: Params,
        mut bundle_content: R,
        supply_default_part_table: bool,
        supply_default_bootloader: bool,
    ) -> anyhow::Result<Self>
    where
        R: Read + Seek,
    {
        let bundle_type = BundleType::iter()
            .find(|&bundle_type| name.ends_with(bundle_type.suffix()))
            .ok_or_else(|| {
                anyhow::anyhow!("Bundle name `{}` does not end with a known suffix", name)
            })?;

        match bundle_type {
            BundleType::Complete => {
                info!("Bundle `{name}` is a ZIP file");
                Self::from_zip_bundle(
                    name,
                    &mut ZipArchive::new(bundle_content)?,
                    supply_default_part_table,
                    supply_default_bootloader,
                )
            }
            BundleType::BinAppImage => {
                info!("Bundle `{name}` is a binary App image");
                let mut bytes = Vec::new();
                bundle_content.read_to_end(&mut bytes)?;

                Self::from_bin_app_image(
                    name,
                    default_params,
                    &bytes,
                    supply_default_part_table,
                    supply_default_bootloader,
                )
            }
            BundleType::ElfAppImage => {
                info!("Bundle `{name}` is an ELF App image");
                let mut bytes = Vec::new();
                bundle_content.read_to_end(&mut bytes)?;

                Self::from_elf_app_image(
                    name,
                    default_params,
                    &bytes,
                    supply_default_part_table,
                    supply_default_bootloader,
                )
            }
        }
    }

    /// Create a new `Bundle` from an ELF application image
    ///
    /// # Arguments
    /// - `name`: The name of the bundle
    /// - `params`: The parameters of the bundle (chip and an optional flash size)
    /// - `app_image`: The content of the ELF application image
    /// - `supply_default_part_table`: Whether to supply the default partition table
    /// - `supply_default_bootloader`: Whether to supply the default bootloader
    pub fn from_elf_app_image(
        name: String,
        params: Params,
        app_image: &[u8],
        supply_default_part_table: bool,
        supply_default_bootloader: bool,
    ) -> anyhow::Result<Self> {
        info!("About to prep the ELF App image bundle `{name}`");

        let app_image =
            Image::new_elf("ota_1".to_string(), flash::elf2bin(app_image, params.chip)?);

        Self::from_parts(
            name,
            params,
            Payload::new(None, supply_default_part_table),
            Payload::new(None, supply_default_bootloader),
            once(app_image),
            Vec::new().into_iter(),
        )
    }

    /// Create a new `Bundle` from a binary application image
    ///
    /// # Arguments
    /// - `name`: The name of the bundle
    /// - `params`: The parameters of the bundle (chip and an optional flash size)
    /// - `app_image`: The content of the binary application image
    /// - `supply_default_part_table`: Whether to supply the default partition table
    /// - `supply_default_bootloader`: Whether to supply the default bootloader
    pub fn from_bin_app_image(
        name: String,
        params: Params,
        app_image: &[u8],
        supply_default_part_table: bool,
        supply_default_bootloader: bool,
    ) -> anyhow::Result<Self> {
        info!("About to prep the binary App image bundle `{name}`");

        let app_image = Image::new("ota_1".to_string(), app_image.to_vec());

        Self::from_parts(
            name,
            params,
            Payload::new(None, supply_default_part_table),
            Payload::new(None, supply_default_bootloader),
            once(app_image),
            Vec::new().into_iter(),
        )
    }

    /// Create a new `Bundle` from a ZIP bundle
    ///
    /// # Arguments
    /// - `name`: The name of the bundle
    /// - `zip`: The ZIP archive containing the bundle content
    /// - `supply_default_part_table`: Whether to supply the default partition table if the partition table is not provided in the bundle
    /// - `supply_default_bootloader`: Whether to supply the default bootloader if the bootloader is not provided in the bundle
    pub fn from_zip_bundle<T>(
        name: String,
        zip: &mut ZipArchive<T>,
        supply_default_part_table: bool,
        supply_default_bootloader: bool,
    ) -> anyhow::Result<Self>
    where
        T: Read + Seek,
    {
        info!("About to prep the ZIP image bundle `{name}`");

        let mut params_str = String::new();
        zip.by_name(Self::PARAMS_FILE_NAME)?
            .read_to_string(&mut params_str)
            .with_context(|| {
                format!(
                    "Loading {} from the ZIP file failed",
                    Self::PARAMS_FILE_NAME
                )
            })?;

        let params: Params = toml::from_str(&params_str)?;

        let part_table_str = zip
            .index_for_name(Self::PART_TABLE_FILE_NAME)
            .map(|index| {
                let mut zip_file = zip.by_index(index).with_context(|| {
                    format!(
                        "Loading `{}` from the ZIP file failed",
                        Self::PARAMS_FILE_NAME
                    )
                })?;

                let mut part_table_str = String::new();

                zip_file
                    .read_to_string(&mut part_table_str)
                    .with_context(|| {
                        format!(
                            "Loading `{}` from the ZIP file failed",
                            Self::PART_TABLE_FILE_NAME
                        )
                    })?;

                Ok::<_, anyhow::Error>(part_table_str)
            })
            .transpose()?;

        let bootloader_image = zip
            .index_for_name(Self::BOOTLOADER_FILE_NAME)
            .map(|index| {
                let mut zip_file = zip.by_index(index).with_context(|| {
                    format!(
                        "Loading `{}` from the ZIP file failed",
                        Self::BOOTLOADER_FILE_NAME
                    )
                })?;

                let mut data = Vec::new();
                zip_file.read_to_end(&mut data).with_context(|| {
                    format!(
                        "Loading `{}` from the ZIP file failed",
                        Self::BOOTLOADER_FILE_NAME
                    )
                })?;

                Ok::<_, anyhow::Error>(Image::new(Self::BOOTLOADER_NAME.to_string(), data))
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
                let mut zip_file = zip
                    .by_name(&file_name)
                    .with_context(|| format!("Loading `{}` from the ZIP file failed", file_name))?;

                let mut data = Vec::new();
                zip_file
                    .read_to_end(&mut data)
                    .with_context(|| format!("Loading `{}` from the ZIP file failed", file_name))?;

                let name = file_name
                    .strip_prefix(Self::IMAGES_PREFIX)
                    .unwrap()
                    .trim_end_matches(Self::BIN_SUFFIX);

                let elf = !file_name.ends_with(Self::BIN_SUFFIX);

                let image = if elf {
                    Image::new(name.to_string(), flash::elf2bin(&data, params.chip)?)
                } else {
                    Image::new(name.to_string(), data)
                };

                Ok(image)
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
                let mut zip_file = zip
                    .by_name(file_name.as_str())
                    .with_context(|| format!("Loading `{}` from the ZIP file failed", file_name))?;

                let mut data = Vec::new();
                zip_file
                    .read_to_end(&mut data)
                    .with_context(|| format!("Loading `{}` from the ZIP file failed", file_name))?;

                Efuse::new(
                    file_name.strip_prefix(Self::EFUSES_PREFIX).unwrap(),
                    Arc::new(data),
                )
            })
            .collect();

        let efuses = efuses?;

        Self::from_parts(
            name,
            params,
            Payload::new(part_table_str.as_deref(), supply_default_part_table),
            Payload::new(bootloader_image, supply_default_bootloader),
            images.into_iter(),
            efuses.into_iter(),
        )
    }

    /// Create a new `Bundle` from the parts of the bundle
    ///
    /// # Arguments
    /// - `name`: The name of the bundle
    /// - `params`: The parameters of the bundle (chip and an optional flash size)
    /// - `part_table_str`: The partition table as a string
    /// - `bootloader`: The bootloader image
    /// - `images`: The images to be flashed to the partitions, where the key is the partition name
    /// - `efuses`: The efuses to be programmed, where the key is the efuse name
    pub fn from_parts(
        name: String,
        params: Params,
        part_table_str: Payload<&str>,
        bootloader: Payload<Image>,
        images: impl Iterator<Item = Image>,
        efuses: impl Iterator<Item = Efuse>,
    ) -> anyhow::Result<Self> {
        /// Helper struct to hold the partition table data
        struct PartTableData {
            /// The parsed partition table
            table: PartitionTable,
            /// The offset of the partition table
            offset: u32,
            /// The image of the partition table
            image: Image,
        }

        impl PartTableData {
            /// Create a new `PartTableData` from the partition table string
            fn new(part_table_str: Payload<&str>) -> anyhow::Result<Option<Self>> {
                let part_table_str =
                    part_table_str.into_option(|| Ok(Bundle::DEFAULT_PART_TABLE))?;

                if let Some(part_table_str) = part_table_str {
                    let table = esp_idf_part::PartitionTable::try_from_str(part_table_str)
                        .context("Parsing CSV partition table failed")?;

                    let offset = table.partitions()[0].offset() - Bundle::PART_TABLE_SIZE as u32;

                    let image = Image::new(
                        Bundle::PART_TABLE_NAME.to_string(),
                        table
                            .to_bin()
                            .context("Converting CSV partition table to binary failed")?,
                    );

                    Ok(Some(PartTableData {
                        table,
                        offset,
                        image,
                    }))
                } else {
                    Ok(None)
                }
            }
        }

        info!("Prepping bundle `{name}` from parts");

        let pt = PartTableData::new(part_table_str)?;

        let mut images = images
            .into_iter()
            .map(|image| (image.name.clone(), image))
            .collect::<HashMap<_, _>>();

        let mut parts_mapping = Vec::new();

        if let Some(image) = bootloader.into_option(|| {
            flash::default_bootloader(params.chip, params.flash_size)
                .map(|bl| Image::new(Self::BOOTLOADER_NAME.to_string(), bl))
        })? {
            parts_mapping.push(PartitionMapping {
                partition: pt.as_ref().map(|pt| {
                    Partition::new(
                        Self::BOOTLOADER_NAME,
                        Type::Custom(0),
                        SubType::Custom(0),
                        params.chip.boot_addr(),
                        pt.offset - params.chip.boot_addr(),
                        false,
                    )
                }),
                image: Some(image),
            });
        }

        if let Some(pt) = pt.as_ref() {
            parts_mapping.push(PartitionMapping {
                partition: Some(Partition::new(
                    Self::PART_TABLE_NAME,
                    Type::Custom(0),
                    SubType::Custom(0),
                    pt.offset,
                    Self::PART_TABLE_SIZE as u32,
                    false,
                )),
                image: Some(pt.image.clone()),
            });

            for partition in pt.table.partitions() {
                let image = if let Entry::Occupied(entry) = images.entry(partition.name()) {
                    let image = entry.remove();

                    if matches!(image.ty, ImageType::Elf) {
                        if matches!(partition.ty(), Type::App) {
                            warn!("ELF image found for partition `{}`, prefer `.bin` files, as they take less space", partition.name());
                        } else {
                            anyhow::bail!("Partition `{}` is not of type 'App', but an ELF image was provided", partition.name());
                        }
                    }

                    Some(image)
                } else {
                    None
                };

                parts_mapping.push(PartitionMapping {
                    partition: Some(partition.clone()),
                    image,
                });
            }
        }

        for image in images.into_values() {
            parts_mapping.push(PartitionMapping {
                partition: None,
                image: Some(image),
            });
        }

        info!("Bundle `{name}` prepared");

        let this = Self {
            name,
            params,
            parts_mapping,
            efuse_mapping: efuses
                .map(|efuse| EfuseMapping {
                    efuse,
                    status: ProvisioningStatus::NotStarted,
                })
                .collect(),
        };

        this.check_part_sizes()?;

        Ok(this)
    }

    /// Add the images and efuses of another bundle into the current bundle
    ///
    /// # Arguments
    /// - `other`: The other bundle to merge into the current bundle
    /// - `ovewrite`: Whether to overwrite the images and efuses of the current bundle
    ///   with the images and efuses of the other bundle
    pub fn add(&mut self, other: Self, overwrite: bool) -> anyhow::Result<()> {
        if self.params != other.params {
            anyhow::bail!("Cannot merge bundles with different parameters");
        }

        let mut other_images = other
            .parts_mapping
            .into_iter()
            .filter_map(|mapping| mapping.image)
            .map(|image| (image.name.clone(), image))
            .collect::<HashMap<_, _>>();

        for mapping in &mut self.parts_mapping {
            let name = mapping
                .partition
                .as_ref()
                .map(|partition| partition.name())
                .unwrap_or_else(|| mapping.image.as_ref().unwrap().name.clone());

            if let Entry::Occupied(entry) = other_images.entry(name.clone()) {
                if mapping.image.is_none() || overwrite {
                    mapping.image = Some(entry.remove());
                } else {
                    anyhow::bail!("Image for mapping `{}` already exists", name);
                }
            }
        }

        let other_efuses = other.efuse_mapping;

        if !overwrite {
            for efuse in &other_efuses {
                for existing_efuse in &self.efuse_mapping {
                    if efuse.efuse.is_same(&existing_efuse.efuse) {
                        anyhow::bail!("Efuse `{}` already exists", efuse.efuse);
                    }
                }
            }
        }

        for image in other_images.into_values() {
            self.parts_mapping.push(PartitionMapping {
                partition: None,
                image: Some(image),
            });
        }

        for efuse in other_efuses {
            if let Some(existing_efuse) = self
                .efuse_mapping
                .iter_mut()
                .find(|existing_efuse| efuse.efuse.is_same(&existing_efuse.efuse))
            {
                *existing_efuse = efuse;
            } else {
                self.efuse_mapping.push(efuse);
            }
        }

        self.name = format!("{}+{}", self.name, other.name);

        self.check_part_sizes()?;

        Ok(())
    }

    pub fn add_empty(&mut self) {
        for mapping in &mut self.parts_mapping {
            if let Some(partition) = mapping.partition.as_ref() {
                if mapping.image.is_none() {
                    mapping.image = Some(Image::new_empty(partition.size() as _));
                }
            }
        }
    }

    /// Return `true` if the bundle is bootable, i.e. has a partition table, a bootloader, and an app image
    pub fn is_bootable(&self) -> bool {
        self.has_part_table() && self.has_bootloader() && self.has_app_image()
    }

    /// Return `true` if the bundle has a partition table
    pub fn has_part_table(&self) -> bool {
        self.parts_mapping
            .iter()
            .any(|mapping| mapping.partition.is_some())
    }

    /// Return `true` if the bundle has a bootloader
    pub fn has_bootloader(&self) -> bool {
        self.parts_mapping.iter().any(|mapping| {
            mapping
                .partition
                .as_ref()
                .map(|partition| partition.name().as_str() == Self::BOOTLOADER_NAME)
                .unwrap_or(false)
        })
    }

    /// Return `true` if the bundle has at least one app image
    pub fn has_app_image(&self) -> bool {
        self.parts_mapping.iter().any(|mapping| {
            mapping
                .partition
                .as_ref()
                .map(|partition| matches!(partition.ty(), Type::App) && mapping.image.is_some())
                .unwrap_or(false)
        })
    }

    /// Get all flash encryption keys (if any)
    pub(crate) fn get_flash_encrypt_keys(&self) -> impl Iterator<Item = &'_ [u8]> + '_ {
        self.efuse_mapping.iter().filter_map(|mapping| {
            if let Efuse::Key {
                block: _,
                key_value,
                purpose,
            } = &mapping.efuse
            {
                (purpose == "XTS_AES_128_KEY").then_some(key_value.as_slice())
            } else {
                None
            }
        })
    }

    /// Get the flash data to be flashed to the device
    pub(crate) fn get_flash_data(&self) -> impl Iterator<Item = FlashData> + '_ {
        self.parts_mapping.iter().filter_map(move |mapping| {
            mapping.partition.as_ref().and_then(|partition| {
                mapping.image.as_ref().map(|image| FlashData {
                    offset: partition.offset(),
                    data: image.data.clone(),
                    encrypted_partition: partition.encrypted()
                        || partition.name() == Self::BOOTLOADER_NAME
                        || partition.name() == Self::PART_TABLE_NAME,
                })
            })
        })
    }

    /// Set the status of all images to the given status
    pub(crate) fn set_status_all(&mut self, status: ProvisioningStatus) -> bool {
        let mut modified = false;

        for mapping in &mut self.parts_mapping {
            if mapping.partition.is_some() {
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

    /// Set the status of the image for the given partition to the given status
    pub(crate) fn set_status(&mut self, part_offset: u32, status: ProvisioningStatus) -> bool {
        let mut modified = false;

        for mapping in &mut self.parts_mapping {
            if let Some(partition) = mapping.partition.as_ref() {
                if partition.offset() == part_offset {
                    if let Some(image) = mapping.image.as_mut() {
                        if image.status != status {
                            image.status = status;
                            modified = true;
                        }
                    }
                }
            }
        }

        modified
    }

    fn check_part_sizes(&self) -> anyhow::Result<()> {
        for mapping in &self.parts_mapping {
            if let Some(partition) = mapping.partition.as_ref() {
                if let Some(image) = mapping.image.as_ref() {
                    let part_len = partition.size() as usize;

                    if image.data.len() > part_len {
                        anyhow::bail!(
                            "Image `{}` is too large for partition `{}` ({}B > {}B)",
                            image.name,
                            partition.name(),
                            image.data.len(),
                            part_len
                        );
                    }
                }
            }
        }

        Ok(())
    }
}

impl Display for Bundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Bundle `{}` {{", self.name)?;
        writeln!(f, "  Parameters: {}", self.params)?;

        writeln!(f, "  Partition {{")?;
        for mapping in &self.parts_mapping {
            writeln!(f, "    {mapping}")?;
        }
        writeln!(f, "  }}")?;

        writeln!(f, "  eFuse {{")?;
        for efuse in &self.efuse_mapping {
            writeln!(f, "    {efuse}")?;
        }
        writeln!(f, "  }}")?;

        writeln!(f, "}}")?;

        Ok(())
    }
}

/// A type for a payload that can be either provided, not providced,
/// or requested to be the default payload for that payload type
///
/// This is used for the partition table and bootloader images,
/// where the default partition table and bootloader are used if requested with `Default`
#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub enum Payload<T> {
    /// No payload provided
    None,
    /// Request default payload
    Default,
    /// Provided payload
    Provided(T),
}

impl<T> Payload<T> {
    /// Create a new `Payload` with the given data and whether to supply the default payload if the data is `None`
    pub fn new(data: Option<T>, supply_default: bool) -> Self {
        match data {
            Some(data) => Self::Provided(data),
            None => {
                if supply_default {
                    Self::Default
                } else {
                    Self::None
                }
            }
        }
    }

    /// Get the data from the payload supplying the default if requested
    fn into_option<F>(self, f: F) -> anyhow::Result<Option<T>>
    where
        F: FnOnce() -> anyhow::Result<T>,
    {
        match self {
            Self::None => Ok(None),
            Self::Default => f().map(Some),
            Self::Provided(data) => Ok(Some(data)),
        }
    }
}

/// The parameters of the bundle
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
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

impl Display for Params {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Chip: {:?}", self.chip)?;

        if let Some(flash_size) = self.flash_size {
            write!(f, ", Flash size: {:?}", flash_size)?;
        }

        Ok(())
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

    /// Convert the `Chip` to a string representation
    /// suitable for usage with the Espressif tools (`esptool.py`, `espefuse.py`)
    pub const fn as_tools_str(&self) -> &str {
        match self {
            Self::Esp32 => "esp32",
            Self::Esp32c2 => "esp32c2",
            Self::Esp32c3 => "esp32c3",
            Self::Esp32c6 => "esp32c6",
            Self::Esp32h2 => "esp32h2",
            Self::Esp32p4 => "esp32p4",
            Self::Esp32s2 => "esp32s2",
            Self::Esp32s3 => "esp32s3",
        }
    }

    /// Convert the `Chip` to a `espflash::targets::Chip` instance
    pub const fn to_flash_chip(self) -> espflash::targets::Chip {
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

impl Display for Chip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_tools_str())
    }
}

/// The data to be flashed to the device
#[derive(Clone, Debug)]
pub struct FlashData {
    /// The offset in the flash memory
    pub offset: u32,
    /// The data to be flashed
    pub data: Arc<Vec<u8>>,
    /// Whether the partition where the data is to be flashed is marked as encrypted
    pub encrypted_partition: bool,
}

impl Display for FlashData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Flash data for addr `0x{:08x}` ({}B)",
            self.offset,
            self.data.len()
        )?;

        if self.encrypted_partition {
            write!(f, " (encrypted)")?;
        }

        Ok(())
    }
}

/// The mapping of a partition to an image
///
/// Such a mapping is created for each partition in the partition table, as well as for the bootloader and the partition table itself
/// If there is no image for a partition, the image is `None`
#[derive(Clone, Debug)]
pub struct PartitionMapping {
    /// The partition
    pub partition: Option<Partition>,
    /// The image to be flashed to the partition; if `None`, the partition will be left empty
    pub image: Option<Image>,
}

impl PartitionMapping {
    /// Get the status of the image
    pub fn status(&self) -> Option<ProvisioningStatus> {
        self.partition
            .is_some()
            .then(|| self.image.as_ref().map(|image| image.status))
            .flatten()
    }
}

impl Display for PartitionMapping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(partition) = &self.partition {
            write!(
                f,
                "{}({}({}) 0x{:08x} {}B",
                partition.name(),
                partition.ty(),
                partition.subtype(),
                partition.offset(),
                partition.size()
            )?;

            if partition.encrypted() {
                write!(f, " encrypted")?;
            }

            write!(f, ")")?;
        } else {
            write!(f, "(none)")?;
        }

        if let Some(image) = &self.image {
            write!(f, " -> {}", image)?;
        } else {
            write!(f, " -> (none)")?;
        }

        Ok(())
    }
}

/// An efuse to be programmed
#[derive(Debug, Clone)]
pub enum Efuse {
    /// A parameter efuse - basically a name and a numeric value
    ///
    /// Useful for programming single-bit efuse values (boolean or not) as well as multi-bit values
    /// which are not keys, key digests or the custom MAC
    ///
    /// For burning those, the equivalent of `espefuse.py burn_efuse` command is used
    Param {
        /// The name of the efuse, as documented here:
        /// https://docs.espressif.com/projects/esptool/en/latest/esp32s3/espefuse/burn-efuse-cmd.html
        name: String,
        /// The value of the efuse, as a numeric value as documented here:
        /// https://docs.espressif.com/projects/esptool/en/latest/esp32s3/espefuse/burn-efuse-cmd.html
        value: u32,
    },
    /// A key efuse - a key value to be programmed
    ///
    /// Useful for programming keys
    ///
    /// For burning those, the equivalent of `espefuse.py burn_key` command is used
    Key {
        /// The block of the key efuse, as documented here:
        /// https://docs.espressif.com/projects/esptool/en/latest/esp32s3/espefuse/burn-key-cmd.html
        block: String,
        /// The key value to be programmed, as documented here:
        /// https://docs.espressif.com/projects/esptool/en/latest/esp32s3/espefuse/burn-key-cmd.html
        key_value: Arc<Vec<u8>>,
        /// The key purpose, as documented here:
        /// https://docs.espressif.com/projects/esptool/en/latest/esp32s3/espefuse/burn-key-cmd.html
        purpose: String,
    },
    /// A key digest efuse - a digest value to be programmed
    ///
    /// Useful for programming key digests (i.e. the Secure Boot V2 RSA key digest)
    ///
    /// For burning those, the equivalent of `espefuse.py burn_digest` command is used
    KeyDigest {
        /// The block of the key digest efuse, as documented here:
        /// https://docs.espressif.com/projects/esptool/en/latest/esp32s3/espefuse/burn-key-digest-cmd.html
        block: String,
        /// The digest value to be programmed (public key in PEM format needs to be provided !!), as documented here:
        /// https://docs.espressif.com/projects/esptool/en/latest/esp32s3/espefuse/burn-key-digest-cmd.html
        digest_value: Arc<Vec<u8>>,
        /// The key digest purpose, as documented here:
        /// https://docs.espressif.com/projects/esptool/en/latest/esp32s3/espefuse/burn-key-digest-cmd.html
        purpose: String,
    },
}

impl Efuse {
    /// Create a new `Efuse` from the given file name and file data
    pub fn new(name: &str, data: Arc<Vec<u8>>) -> anyhow::Result<Self> {
        let mut parts = name.split('-');

        let ty = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("Invalid efuse name `{name}`"))?;

        match ty.to_ascii_lowercase().as_str() {
            "param" => {
                let name = parts
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("Invalid efuse name `{name}`"))?;

                let value_str = parts
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("Invalid efuse value for name `{name}"))?;

                let value_str_num = value_str.strip_prefix("0x").ok_or_else(|| {
                    anyhow::anyhow!("Invalid efuse value `{value_str}` for name `{name}")
                })?;

                let value = u32::from_str_radix(value_str_num, 16)?;

                if parts.next().is_some() {
                    anyhow::bail!("Invalid efuse name `{name}`");
                }

                if !data.is_empty() {
                    anyhow::bail!("Invalid efuse data for efuse `{name}`: {} bytes provided, but no data expected", data.len());
                }

                Ok(Self::Param {
                    name: name.to_string(),
                    value,
                })
            }
            "key" | "keydigest" => {
                let block = parts
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("Invalid efuse name `{name}`"))?;
                let purpose = parts
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("Invalid efuse name `{name}`"))?;

                if parts.next().is_some() {
                    anyhow::bail!("Invalid efuse name `{name}`");
                }

                if data.is_empty() {
                    anyhow::bail!("Invalid efuse data for efuse `{name}`: empty");
                }

                if ty == "key" {
                    Ok(Self::Key {
                        block: block.to_string(),
                        key_value: data,
                        purpose: purpose.to_string(),
                    })
                } else {
                    Ok(Self::KeyDigest {
                        block: block.to_string(),
                        digest_value: data,
                        purpose: purpose.to_string(),
                    })
                }
            }
            _ => anyhow::bail!("Invalid efuse type `{ty}`"),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Param { name, .. } => name,
            Self::Key { block, .. } => block,
            Self::KeyDigest { block, .. } => block,
        }
    }

    pub fn is_same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Param { name: name1, .. }, Self::Param { name: name2, .. }) => name1 == name2,
            (Self::Key { block: block1, .. }, Self::Key { block: block2, .. }) => block1 == block2,
            (Self::KeyDigest { block: block1, .. }, Self::KeyDigest { block: block2, .. }) => {
                block1 == block2
            }
            _ => false,
        }
    }
}

impl Display for Efuse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Param { name, value } => write!(f, "param-{}-{:08x}", name, value),
            Self::Key { block, purpose, .. } => write!(f, "key-{}-{}", block, purpose),
            Self::KeyDigest { block, purpose, .. } => write!(f, "keydigest-{}-{}", block, purpose),
        }
    }
}

#[derive(Clone, Debug)]
pub struct EfuseMapping {
    /// The efuse
    pub efuse: Efuse,
    /// The image to be flashed to the partition; if `None`, the partition will be left empty
    pub status: ProvisioningStatus,
}

impl Display for EfuseMapping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.efuse)
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ImageType {
    /// An ELF image
    Elf,
    /// A binary image
    Binary,
    /// An empty space (0xff bytes)
    Empty,
}

impl Display for ImageType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Elf => write!(f, "ELF"),
            Self::Binary => write!(f, "Binary"),
            Self::Empty => write!(f, "Empty"),
        }
    }
}

/// An image to be flashed to some partition
#[derive(Debug, Clone)]
pub struct Image {
    /// The name of the image
    pub name: String,
    /// Image type
    /// Only necessary to know for some sanity checks done during bundle loading,
    /// as in trying to associate an ELF image to a non-App partition
    /// as well as for the UI
    pub ty: ImageType,
    /// The data of the image
    pub data: Arc<Vec<u8>>,
    /// The status of the image flashing
    pub status: ProvisioningStatus,
}

impl Image {
    /// Create a new `Image` from the given binary data, where the binary data
    /// was not extracted from an ELF file
    pub fn new(name: String, data: Vec<u8>) -> Self {
        Self {
            name,
            ty: ImageType::Binary,
            data: Arc::new(data),
            status: ProvisioningStatus::NotStarted,
        }
    }

    /// Create a new `Image` from the given binary data, where the binary data
    /// was extracted from an ELF file
    pub fn new_elf(name: String, data: Vec<u8>) -> Self {
        Self {
            name,
            ty: ImageType::Elf,
            data: Arc::new(data),
            status: ProvisioningStatus::NotStarted,
        }
    }

    /// Create a new `Image` with empty space of the given size
    pub fn new_empty(size: usize) -> Self {
        Self {
            name: "(Empty)".into(),
            ty: ImageType::Empty,
            data: Arc::new(empty_space(size)),
            status: ProvisioningStatus::NotStarted,
        }
    }
}

impl Display for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({}B", self.name, self.data.len())?;

        match self.ty {
            ImageType::Binary => (),
            ImageType::Elf => write!(f, " ELF")?,
            ImageType::Empty => write!(f, " Empty")?,
        }

        write!(f, ")")
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
    InProgress(Option<u8>),
    /// The provisioning process has been completed
    Done,
}

impl Display for ProvisioningStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotStarted => write!(f, "Not started"),
            Self::Pending => write!(f, "Pending"),
            Self::InProgress(progress) => {
                if let Some(progress) = progress {
                    write!(f, "In progress ({:.0}%)", *progress as f64)
                } else {
                    write!(f, "In progress")
                }
            }
            Self::Done => write!(f, "Done"),
        }
    }
}
