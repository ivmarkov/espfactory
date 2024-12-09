use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bitflags::bitflags;

use serde::Deserialize;

extern crate alloc;

#[derive(Clone, Debug)]
pub struct Bundle {
    pub name: String,
    pub partitions: Vec<Partition>,
    pub bootloader: Option<Image>,
    pub images: Vec<Image>,
    pub efuses: Vec<Efuse>,
}

#[derive(Clone, Debug)]
pub struct Image {
    pub file_name: String,
    pub name: String,
    pub data: Arc<Vec<u8>>,
}

impl Image {
    pub fn any_size_string(size: usize) -> String {
        format!(
            "{}KB (0x{:06x})",
            size / 1024 + if size % 1024 > 0 { 1 } else { 0 },
            size
        )
    }

    pub fn size_string(&self) -> String {
        Self::any_size_string(self.data.len())
    }
}

#[derive(Clone, Debug)]
pub struct Efuse {
    pub file_name: String,
    pub name: String, // TODO
    pub data: Arc<Vec<u8>>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize)]
pub enum PartitionType {
    App,
    Data,
    Factory,
    OTA,
    Phy,
    Nvs,
    OtaData,
    Unknown,
}

impl PartitionType {
    pub const fn as_str(&self) -> &str {
        match self {
            Self::App => "app",
            Self::Data => "data",
            Self::Factory => "factory",
            Self::OTA => "ota",
            Self::Phy => "phy",
            Self::Nvs => "nvs",
            Self::OtaData => "ota_data",
            Self::Unknown => "unknown",
        }
    }
}

bitflags! {
    #[derive(Debug, Clone)]
    pub struct PartitionFlags: u32 {
        const ENCRYPTED = 1 << 0;
    }
}

#[derive(Debug, Clone)]
pub struct Partition {
    pub name: String,
    pub part_type: PartitionType,
    pub part_subtype: String,
    pub offset: usize,
    pub size: usize,
    pub flags: PartitionFlags,
}

impl Partition {
    pub fn offset_string(&self) -> String {
        Self::any_offset_string(self.offset)
    }

    pub fn size_string(&self) -> String {
        Self::any_size_string(self.size)
    }

    pub fn any_offset_string(offset: usize) -> String {
        format!("0x{:06x}", offset)
    }

    pub fn any_size_string(size: usize) -> String {
        format!(
            "{}KB (0x{:06x})",
            size / 1024 + if size % 1024 > 0 { 1 } else { 0 },
            size
        )
    }
}
