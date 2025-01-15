use std::io::Write;
use std::path::PathBuf;

use url::Url;

pub mod dir;
pub mod file;
pub mod http;
#[cfg(feature = "s3")]
pub mod s3;

/// Supported bundle types
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum BundleType {
    /// A real bundle
    ///
    /// This is essentially a ZIP file with the following content:
    /// /params.toml (required)         - a TOML file with the chip and optinal flash size parameters
    /// /bootloader.bin (optional)      - a binary file with the bootloader
    ///                                   if missing, a default, unsigned bootloader will be flashed
    /// /partition-table.csv (optional) - a CSV file with the partition table
    ///                                   if missing, a default partition table will be flashedm with lyaout in `Bundle::PARTITION_TABLE`
    /// /images/<partXXX> (optional)    - a binary (.bin) or ELF file to be flashed to partition `partXXX` (image name should match partition name);
    ///                                   if missing, the partition will be left empty
    /// /images/<partYYY> (optional)    - a binary (.bin) or ELF file to be flashed to partition `partYYY` (image name should match partition name);
    ///                                   if missing, the partition will be left empty
    /// ...
    /// /efuses/<efuse_name> (optional) - TBD a binary file with an efuse content
    Complete,
    /// Binary application image
    ///
    /// This is a single binary file to be flashed to the device
    ///
    /// Since bootloader and partition table are not provided, the default ones would be used
    /// The image is flashed to the first partition which either is of type `factory`, or if a factory partition is not found,
    /// to the first OTA partition
    BinAppImage,
    /// ELF application image
    ///
    /// This is a single ELF file to be flashed to the device
    ///
    /// Prior to flashing, the ELF file is converted to a binary image.
    /// Since bootloader and partition table are not provided, the default ones would be used
    /// The image is flashed to the first partition which either is of type `factory`, or if a factory partition is not found,
    /// to the first OTA partition
    ElfAppImage,
}

impl BundleType {
    /// Iterate over all supported bundle types
    pub fn iter() -> impl Iterator<Item = Self> {
        [Self::Complete, Self::BinAppImage, Self::ElfAppImage].into_iter()
    }

    /// Get the file name for the bundle with the given ID
    pub fn file(&self, id: &str) -> String {
        format!("{}{}", id, self.suffix())
    }

    /// Get the file name suffix for the bundle type
    pub const fn suffix(&self) -> &str {
        match self {
            Self::Complete => ".bundle",
            Self::BinAppImage => ".bin",
            Self::ElfAppImage => "",
        }
    }
}

/// A trait that loads a bundle from a bundle source
pub trait BundleLoader {
    /// Load a bundle from a bundle source
    ///
    /// # Arguments
    /// - `write` - a writer to write the bundle to
    /// - `id` - an optional ID of the bundle to load, where the ID is usually a PCB number, or a device ID number
    ///          (see `BundleIdentification`)
    ///          if provided, then the bundle with the given ID is loaded and the bundle is not removed from the source
    ///          if not provided, then a random bundle is loaded and the bundle is removed from the source
    ///
    /// # Returns
    /// The name of the loaded bundle
    async fn load<W>(&mut self, write: W, id: Option<&str>) -> anyhow::Result<String>
    where
        W: Write;
}

impl<T> BundleLoader for &mut T
where
    T: BundleLoader,
{
    async fn load<W>(&mut self, write: W, id: Option<&str>) -> anyhow::Result<String>
    where
        W: Write,
    {
        (*self).load(write, id).await
    }
}

/// Wrapper enum for the loaders supported OOTB
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Loader {
    /// Load a bundle from a file
    File(file::FileLoader),
    /// Load bundles from a directory
    Dir(dir::DirLoader),
    /// Load bundles from an HTTP(s) server
    Http(http::HttpLoader),
    /// Load bundles from an S3 bucket
    #[cfg(feature = "s3")]
    S3(s3::S3Loader),
}

impl Loader {
    pub fn new(url: &Url, delete_after_load_allowed: bool) -> anyhow::Result<Self> {
        match url.scheme() {
            "file" => Ok(Self::File(file::FileLoader::new(PathBuf::from(
                url.path().to_string(),
            )))),
            "dir" | "dird" if delete_after_load_allowed => Ok(Self::Dir(dir::DirLoader::new(
                PathBuf::from(url.path().to_string()),
                matches!(url.scheme(), "dird"),
                None,
            ))),
            "http" | "https" => Ok(Self::Http(http::HttpLoader::new(
                url.as_str().to_string(),
                None,
                false,
                true,
                None,
            ))),
            #[cfg(feature = "s3")]
            "s3" | "s3d" if delete_after_load_allowed => {
                let bucket = url
                    .host_str()
                    .ok_or_else(|| anyhow::anyhow!("No bucket provided in URL: {}", url))?
                    .to_string();
                let path = url.path().trim_matches('/');
                let path = (!path.is_empty()).then(|| path.to_string());

                Ok(Self::S3(s3::S3Loader::new(
                    None,
                    bucket,
                    path,
                    matches!(url.scheme(), "s3d"),
                    None,
                    None,
                )))
            }
            _ => anyhow::bail!("Unsupported bundle load URL: {url}"),
        }
    }
}

impl BundleLoader for Loader {
    async fn load<W>(&mut self, write: W, id: Option<&str>) -> anyhow::Result<String>
    where
        W: std::io::Write,
    {
        match self {
            Self::File(loader) => loader.load(write, id).await,
            Self::Dir(loader) => loader.load(write, id).await,
            Self::Http(loader) => loader.load(write, id).await,
            #[cfg(feature = "s3")]
            Self::S3(loader) => loader.load(write, id).await,
        }
    }
}
