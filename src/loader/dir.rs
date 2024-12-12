use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::Context;

use super::BundleLoader;

#[derive(Debug, Clone)]
pub struct DirLoader {
    path: PathBuf,
}

impl DirLoader {
    pub const fn new(path: PathBuf) -> Self {
        Self { path }
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

            if id.is_none() {
                fs::remove_file(&path).context("Removing the bundle failed")?;
            }

            Ok(path.file_name().unwrap().to_str().unwrap().to_string())
        } else {
            anyhow::bail!("No files found in bundles' directory")
        }
    }
}
