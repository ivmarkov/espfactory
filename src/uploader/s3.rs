use core::fmt::{self, Display};

use std::io::{self, Read, Seek, SeekFrom, Write};

use anyhow::Context;
use aws_sdk_s3::primitives::ByteStream;

use log::info;

use tempfile::tempfile;

use crate::uploader::log_name;

use super::BundleLogsUploader;

/// Re-export the `aws-config` crate as a module so that the user
/// does not have to depend on the `aws-config` crate directly
pub mod aws_config {
    pub use ::aws_config::*;
}

/// A logs uploader that uploads the logs to an S3 bucket and an optional prefix.
#[derive(Debug, Clone)]
pub struct S3LogsUploader {
    config: Option<aws_config::SdkConfig>,
    logs_upload_bucket: String,
    logs_upload_prefix: Option<String>,
}

impl S3LogsUploader {
    /// Creates a new `S3LogsUploader` instance
    ///
    /// # Arguments
    /// - `config` - The optional AWS SDK configuration
    /// - `logs_upload_bucket` - The name of the S3 bucket to upload the logs to
    /// - `logs_upload_prefix` - The optional prefix to use when uploading the logs
    pub const fn new(
        config: Option<aws_config::SdkConfig>,
        logs_upload_bucket: String,
        logs_upload_prefix: Option<String>,
    ) -> Self {
        Self {
            config,
            logs_upload_bucket,
            logs_upload_prefix,
        }
    }
}

impl BundleLogsUploader for S3LogsUploader {
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
                "About to upload logs `{log_name}` for Bundle ID `{bundle_id}` to S3 bucket `{}`...",
                BucketWithPrefix::new(&self.logs_upload_bucket, self.logs_upload_prefix.as_deref())
            );
        } else {
            info!(
                "About to uploads logs `{log_name}` to S3 bucket `{}`...",
                BucketWithPrefix::new(&self.logs_upload_bucket, self.logs_upload_prefix.as_deref())
            );
        }

        let config = if let Some(config) = self.config.as_ref() {
            config.clone()
        } else {
            aws_config::load_from_env().await
        };

        let client = aws_sdk_s3::Client::new(&config);

        let key = self
            .logs_upload_prefix
            .as_deref()
            .map(|prefix| format!("{prefix}/{log_name}"))
            .unwrap_or(log_name.clone());

        read.seek(io::SeekFrom::Start(0))
            .context("Saving the bundle log failed")?;

        let mut temp_file = tempfile().context("Uploading the bundle log failed")?;
        std::io::copy(&mut read, &mut temp_file).context("Uploading the bundle log failed")?;

        temp_file
            .flush()
            .context("Uploading the bundle log failed")?;
        temp_file
            .seek(SeekFrom::Start(0))
            .context("Uploading the bundle log failed")?;

        client
            .put_object()
            .bucket(&self.logs_upload_bucket)
            .key(key)
            .body(
                ByteStream::read_from()
                    .file(temp_file.into())
                    .build()
                    .await
                    .context("Uploading the bundle log failed")?,
            )
            .send()
            .await
            .context("Uploading the bundle log failed")?;

        info!("Logs `{log_name}` uploaded");

        Ok(())
    }
}

#[derive(Debug)]
struct BucketWithPrefix<'a> {
    bucket: &'a str,
    prefix: Option<&'a str>,
}

impl<'a> BucketWithPrefix<'a> {
    const fn new(bucket: &'a str, prefix: Option<&'a str>) -> Self {
        Self { bucket, prefix }
    }
}

impl Display for BucketWithPrefix<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(prefix) = self.prefix {
            write!(f, "{}/{}", self.bucket, prefix)
        } else {
            write!(f, "{}", self.bucket)
        }
    }
}
