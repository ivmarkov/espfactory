use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
};

pub trait BundleLoader {
    async fn load<W>(&mut self, write: W) -> anyhow::Result<String>
    where
        W: Write;
}

impl<T> BundleLoader for &mut T
where
    T: BundleLoader,
{
    async fn load<W>(&mut self, write: W) -> anyhow::Result<String>
    where
        W: Write,
    {
        (*self).load(write).await
    }
}

pub struct DirLoader(PathBuf);

impl DirLoader {
    pub const fn new(path: PathBuf) -> Self {
        Self(path)
    }
}

impl BundleLoader for DirLoader {
    async fn load<W>(&mut self, mut write: W) -> anyhow::Result<String>
    where
        W: Write,
    {
        let dir = fs::read_dir(&self.0)?;

        for entry in dir {
            let entry = entry?;
            let path = entry.path();

            if path.is_file() {
                {
                    let mut file = fs::File::open(&path)?;

                    io::copy(&mut file, &mut write)?;
                }

                fs::remove_file(&path)?;

                return Ok(path.file_name().unwrap().to_str().unwrap().to_string());
            }
        }

        anyhow::bail!("No files found in directory")
    }
}
