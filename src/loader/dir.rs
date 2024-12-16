use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::Context;

use super::BundleLoader;

/// A loader that reads bundles from a directory.
///
/// The directory is expected to have a flat structure, containing a bunch of files, where
/// each file represents a bundle with a unique name and an extension matching one of the ones returned by
/// `BundleType::suffix()`
///
/// If the bundles are loaded by ID, then the bundle name is assumed to be the ID with the corresponding extension
/// i.e. `<ID>.bundle`, `<ID>.bin`, or `<ID>`. Otherwise, each file in the directory is treated as a bundle as long as
/// it has an extension matching one of the ones returned by `BundleType::suffix()`, and the loader just loads (and removes)
/// a random file from the directory
#[derive(Debug, Clone)]
pub struct DirLoader {
    path: PathBuf,
    delete_after_load: bool,
}

impl DirLoader {
    /// Creates a new `DirLoader`
    ///
    /// Arguments
    /// - `path`: The path to the directory to load the bundles from
    /// - `delete_after_load`: A flag indicating whether the loaded bundle should be deleted from the directory after loading
    ///   Only used when a bundle is loaded without a supplied ID (i.e. a random bundle)
    pub const fn new(path: PathBuf, delete_after_load: bool) -> Self {
        Self {
            path,
            delete_after_load,
        }
    }
}

impl BundleLoader for DirLoader {
    async fn load<W>(&mut self, mut write: W, id: Option<&str>) -> anyhow::Result<String>
    where
        W: Write,
    {
        let file_name = fs::read_dir(&self.path)
            .context("Cannot open the bundles' directory")?
            .find_map(|entry| {
                (move || {
                    let entry = entry.context("Error when reading the bundles' directory")?;
                    let path = entry.path();

                    let mut matches = false;

                    if path.is_file() {
                        if let Some(file_name) =
                            path.file_name().and_then(|file_name| file_name.to_str())
                        {
                            if let Some(id) = id {
                                if file_name == format!("{id}.bin")
                                    || file_name == format!("{id}.bundle")
                                    || file_name == id
                                {
                                    matches = true;
                                }
                            } else {
                                matches = true;
                            }
                        }
                    }

                    Ok::<_, anyhow::Error>(matches.then_some(path))
                })()
                .transpose()
            })
            .transpose()?;

        if let Some(path) = file_name {
            let mut file = fs::File::open(&path).context("Loading the bundle failed")?;

            io::copy(&mut file, &mut write).context("Copying the bundle failed")?;

            if id.is_none() && self.delete_after_load {
                fs::remove_file(&path).context("Removing the bundle failed")?;
            }

            // TODO
            Ok(path.file_name().unwrap().to_str().unwrap().to_string())
        } else {
            anyhow::bail!("No files found in bundles' directory")
        }
    }
}
