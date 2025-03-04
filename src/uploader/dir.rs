use std::fs::File;
use std::io::{self, Read, Seek};
use std::path::PathBuf;

use anyhow::Context;

use log::info;

use crate::uploader::log_name;

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
        bundle_id: Option<&str>,
        bundle_name: &str,
    ) -> anyhow::Result<()>
    where
        R: Read + Seek,
    {
        let log_name = log_name(bundle_id, bundle_name);

        if let Some(bundle_id) = bundle_id {
            info!(
                "About to save logs `{log_name}` for Bundle ID `{bundle_id}` to directory `{}`...",
                self.logs_path.display()
            );
        } else {
            info!(
                "About to save logs `{log_name}` to directory `{}`...",
                self.logs_path.display()
            )
        }

        read.seek(io::SeekFrom::Start(0))
            .context("Saving the bundle log failed")?;

        let mut file =
            File::create(self.logs_path.join(&log_name)).context("Saving the bundle log failed")?;
        io::copy(&mut read, &mut file).context("Saving the bundle log failed")?;

        info!("Logs `{log_name}` uploaded");

        Ok(())
    }
}
