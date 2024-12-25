use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::Context;

use log::info;

use super::BundleLoader;

/// A loader that reads bundles from a single file.
///
/// Note that this loader does not support loading bundles by ID, as it only reads from a single file.
/// The loader also does not support deleting the file after loading the bundle.
#[derive(Debug, Clone)]
pub struct FileLoader {
    path: PathBuf,
}

impl FileLoader {
    /// Creates a new `FileLoader`
    ///
    /// Arguments
    /// - `path`: The path to the bundle file
    pub const fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl BundleLoader for FileLoader {
    async fn load<W>(&mut self, mut write: W, id: Option<&str>) -> anyhow::Result<String>
    where
        W: Write,
    {
        if id.is_some() {
            anyhow::bail!("Loading a bundle by ID is not supported by the file loader");
        }

        info!("About to load bundle file `{}`...", self.path.display());

        if !self.path.exists() {
            anyhow::bail!("Bundle file `{}` does not exist", self.path.display());
        }

        if !self.path.is_file() {
            anyhow::bail!("Bundle file `{}` is not a file", self.path.display());
        }

        let mut file = fs::File::open(&self.path).context("Loading the bundle failed")?;

        io::copy(&mut file, &mut write).context("Loading the bundle failed")?;

        info!(
            "Loaded bundle `{}`",
            self.path.file_name().unwrap().to_str().unwrap_or("???")
        );

        Ok(self.path.file_name().unwrap().to_str().unwrap().to_string())
    }
}
