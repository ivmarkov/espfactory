use core::fmt::{self, Display};

use std::io::{Read, Seek, SeekFrom, Write};

use anyhow::Context;
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::{error::SdkError, primitives::ByteStream};

use log::info;

use tempfile::tempfile;

use super::{BundleLoader, BundleType};

/// Re-export the `aws-config` crate as a module so that the user
/// does not have to depend on the `aws-cponfig` crate directly
pub mod aws_config {
    pub use ::aws_config::*;
}

/// A loader that reads bundles from an S3 bucket and an optional prefix.
///
/// The loader will attempt to load a bundle by ID, or if no ID is provided, it will load the first bundle found in the bucket, by listing
/// the contents of the bucket using an S3 `list_objects_v2` operation.
///
/// The bundle is loaded by reading the object data from the bucket using an S3 `get_object` operation.
/// The object key is constructed as follows:
/// - If the `id` argument is present when calling `load`, then the key is [<optional-prefix>/]<ID>[.<suffix>]
///   where `<suffix>` is one of the suffixes returned by `BundleType::suffix()`, examined in order of the variants of `BundleType`
/// - If the `id` argument is not present when calling `load`, then the loader will list the contents of the bucket and load the first bundle  
///   with a suffix matching one of the suffixes returned by `BundleType::suffix()`, examined in order of the variants of `BundleType`
///   Furthermore, if the `delete_after_load` flag is set to `true`, then the loader will delete the loaded bundle from the bucket
#[derive(Debug, Clone)]
pub struct S3Loader {
    config: Option<aws_config::SdkConfig>,
    load_bucket: String,
    load_prefix: Option<String>,
    delete_after_load: bool,
    logs_upload_bucket: Option<String>,
    logs_upload_prefix: Option<String>,
}

impl S3Loader {
    pub fn new_from_path(
        config: Option<aws_config::SdkConfig>,
        load_path: String,
        delete_after_load: bool,
        logs_upload_path: Option<String>,
    ) -> Self {
        let load_path = load_path.trim_matches('/');
        let (load_bucket, load_prefix) = if let Some(split) = load_path.find('/') {
            let (load_bucket, load_prefix) = load_path.split_at(split);

            (load_bucket, Some(load_prefix[1..].to_string()))
        } else {
            (load_path, None)
        };

        let (logs_upload_bucket, logs_upload_prefix) =
            if let Some(logs_upload_path) = logs_upload_path {
                let logs_upload_path = logs_upload_path.trim_matches('/');

                if let Some(split) = logs_upload_path.find('/') {
                    let (logs_upload_bucket, logs_upload_prefix) = logs_upload_path.split_at(split);

                    (
                        Some(logs_upload_bucket.to_string()),
                        Some(logs_upload_prefix[1..].to_string()),
                    )
                } else {
                    (Some(logs_upload_path.to_string()), None)
                }
            } else {
                (None, None)
            };

        Self::new(
            config,
            load_bucket.to_string(),
            load_prefix.map(|p| p.to_string()),
            delete_after_load,
            logs_upload_bucket,
            logs_upload_prefix,
        )
    }

    /// Creates a new `S3Loader` instance
    ///
    /// # Arguments
    /// - `bucket`: The name of the S3 bucket to load the bundles from
    /// - `prefix_key`: An optional prefix key to use when loading the bundles
    /// - `delete_after_load`: A flag indicating whether the loaded bundle should be deleted from the bucket after loading
    ///   Used only when loading a random bundle (i.e., the `id` argument when calling `load` is not provided)
    pub const fn new(
        config: Option<aws_config::SdkConfig>,
        load_bucket: String,
        load_prefix: Option<String>,
        delete_after_load: bool,
        logs_upload_bucket: Option<String>,
        logs_upload_prefix: Option<String>,
    ) -> Self {
        Self {
            config,
            load_bucket,
            load_prefix,
            delete_after_load,
            logs_upload_bucket,
            logs_upload_prefix,
        }
    }
}

impl BundleLoader for S3Loader {
    async fn load<W>(&mut self, mut write: W, id: Option<&str>) -> anyhow::Result<String>
    where
        W: Write,
    {
        if let Some(id) = id {
            info!(
                "About to fetch a bundle with ID `{id}` from S3 bucket `{}`...",
                BucketWithPrefix::new(&self.load_bucket, self.load_prefix.as_deref())
            );
        } else {
            info!(
                "About to fetch a random bundle from S3 bucket `{}`...",
                BucketWithPrefix::new(&self.load_bucket, self.load_prefix.as_deref())
            );
        }

        let config = if let Some(config) = self.config.as_ref() {
            config.clone()
        } else {
            aws_config::load_from_env().await
        };

        let client = aws_sdk_s3::Client::new(&config);

        if let Some(id) = id {
            for bundle_type in BundleType::iter() {
                let bundle_name = bundle_type.file(id);
                let key = self
                    .load_prefix
                    .as_deref()
                    .map(|prefix| format!("{}/{}", prefix, bundle_name))
                    .unwrap_or(bundle_name.clone());

                let result = client
                    .get_object()
                    .bucket(&self.load_bucket)
                    .key(&key)
                    .send()
                    .await;

                match result {
                    Ok(mut object_data) => {
                        if object_data.delete_marker().unwrap_or(false) {
                            continue;
                        }

                        while let Some(bytes) = object_data.body.try_next().await? {
                            write.write_all(&bytes)?;
                        }

                        return Ok(bundle_name);
                    }
                    Err(SdkError::ServiceError(err))
                        if matches!(err.err(), GetObjectError::NoSuchKey(_)) =>
                    {
                        continue
                    }
                    Err(other) => Err(other).context("Loading the bundle failed")?,
                }
            }
        } else {
            let mut continuation_token = None;

            loop {
                let mut builder = client.list_objects_v2().bucket(&self.load_bucket);

                if let Some(continuation_token) = continuation_token {
                    builder = builder.continuation_token(continuation_token);
                }

                if let Some(prefix) = &self.load_prefix {
                    builder = builder.prefix(prefix);
                }

                let resp = builder.send().await?;

                for object_desc in resp.contents() {
                    if let Some(key) = object_desc.key() {
                        if BundleType::iter().any(|bundle_type| key.ends_with(bundle_type.suffix()))
                        {
                            let mut object_data = client
                                .get_object()
                                .bucket(&self.load_bucket)
                                .key(key)
                                .send()
                                .await
                                .context("Loading the bundle failed")?;

                            while let Some(bytes) = object_data.body.try_next().await? {
                                write
                                    .write_all(&bytes)
                                    .context("Loading the bundle failed")?;
                            }

                            let bundle_name = key.split('/').last().unwrap_or(key).to_string();

                            if id.is_none() && self.delete_after_load {
                                client
                                    .delete_object()
                                    .bucket(&self.load_bucket)
                                    .key(key)
                                    .send()
                                    .await
                                    .context("Deleting the bundle after loading failed")?;
                            }

                            info!("Loaded bundle `{}`", bundle_name);

                            return Ok(bundle_name);
                        }
                    }
                }

                if let Some(cont) = resp.next_continuation_token() {
                    continuation_token = Some(cont.to_string());
                } else {
                    break;
                }
            }
        }

        if let Some(id) = id {
            anyhow::bail!("No bundle found for ID `{id}`")
        } else {
            anyhow::bail!("No bundles found in the bucket")
        }
    }

    async fn upload_logs<R>(
        &mut self,
        mut read: R,
        id: Option<&str>,
        name: &str,
    ) -> anyhow::Result<()>
    where
        R: Read,
    {
        let Some(logs_upload_bucket) = self.logs_upload_bucket.as_deref() else {
            return Ok(());
        };

        if let Some(id) = id {
            info!(
                "About to upload logs `{name}` for ID `{id}` to S3 bucket `{}`...",
                BucketWithPrefix::new(logs_upload_bucket, self.logs_upload_prefix.as_deref())
            );
        } else {
            info!(
                "About to uploads logs `{name}` to S3 bucket `{}`...",
                BucketWithPrefix::new(logs_upload_bucket, self.logs_upload_prefix.as_deref())
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
            .map(|prefix| format!("{prefix}/{name}.log.zip"))
            .unwrap_or(format!("{name}.log.zip"));

        let mut temp_file = tempfile()?;
        std::io::copy(&mut read, &mut temp_file)?;

        temp_file.flush()?;
        temp_file.seek(SeekFrom::Start(0))?;

        client
            .put_object()
            .bucket(logs_upload_bucket)
            .key(key)
            .body(ByteStream::read_from().file(temp_file.into()).build().await?)
            .send()
            .await
            .context("Uploading bundle logs failed")?;

        info!("Logs `{name}` uploaded");

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
