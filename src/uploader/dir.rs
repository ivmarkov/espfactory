use std::fs::File;
use std::io::{self, Read, Seek};
use std::path::PathBuf;

use anyhow::Context;

use log::info;

use super::BundleLogsUploader;

/// A logs uploader that uploads logs to a directory.
#[derive(Debug, Clone)]
pub struct DirLogsUploader {
    logs_path: PathBuf,
}

impl DirLogsUploader {
    /// Creates a new `DirLogsUploader`
    ///
    /// Arguments
    /// - `logs_path`: The path to the directory to save the logs to
    pub const fn new(logs_path: PathBuf) -> Self {
        Self { logs_path }
    }
}

impl BundleLogsUploader for DirLogsUploader {
    async fn upload_logs<R>(
        &mut self,
        mut read: R,
        id: Option<&str>,
        name: &str,
    ) -> anyhow::Result<()>
    where
        R: Read + Seek,
    {
        if let Some(id) = id {
            info!(
                "About to save logs `{name}.log.zip` for ID `{id}` to directory `{}`...",
                self.logs_path.display()
            );
        } else {
            info!(
                "About to save logs `{name}.log.zip` to directory `{}`...",
                self.logs_path.display()
            )
        }

        read.seek(io::SeekFrom::Start(0))
            .context("Saving the bundle log failed")?;

        let mut file = File::create(self.logs_path.join(format!("{}.log.zip", name)))
            .context("Saving the bundle log failed")?;
        io::copy(&mut read, &mut file).context("Saving the bundle log failed")?;

        info!("Logs `{name}.log.zip` uploaded");

        Ok(())
    }
}
