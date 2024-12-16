use std::io::Write;

use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::get_object::GetObjectError;

use super::{BundleLoader, BundleType};

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
    bucket: String,
    prefix: Option<String>,
    delete_after_load: bool,
}

impl S3Loader {
    pub fn new_from_path(path: String, delete_after_load: bool) -> Self {
        let path = path.trim_matches('/');
        let (bucket, prefix) = if let Some(split) = path.find('/') {
            let (bucket, prefix) = path.split_at(split);

            (bucket, Some(prefix))
        } else {
            (path, None)
        };

        Self::new(
            bucket.to_string(),
            prefix.map(|p| p.to_string()),
            delete_after_load,
        )
    }

    /// Creates a new `S3Loader` instance
    ///
    /// # Arguments
    /// - `bucket`: The name of the S3 bucket to load the bundles from
    /// - `prefix_key`: An optional prefix key to use when loading the bundles
    /// - `delete_after_load`: A flag indicating whether the loaded bundle should be deleted from the bucket after loading
    ///   Used only when loading a random bundle (i.e., the `id` argument when calling `load` is not provided)
    pub const fn new(bucket: String, prefix: Option<String>, delete_after_load: bool) -> Self {
        Self {
            bucket,
            prefix,
            delete_after_load,
        }
    }
}

impl BundleLoader for S3Loader {
    async fn load<W>(&mut self, mut write: W, id: Option<&str>) -> anyhow::Result<String>
    where
        W: Write,
    {
        let config = aws_config::load_from_env().await;
        let client = aws_sdk_s3::Client::new(&config);

        if let Some(id) = id {
            for bundle_type in BundleType::iter() {
                let bundle_name = bundle_type.file(id);
                let key = self
                    .prefix
                    .as_deref()
                    .map(|prefix| format!("{}/{}", prefix, bundle_name))
                    .unwrap_or(bundle_name.clone());

                let result = client
                    .get_object()
                    .bucket(&self.bucket)
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
                    Err(other) => Err(other)?,
                }
            }
        } else {
            let mut continuation_token = None;

            loop {
                let mut builder = client.list_objects_v2().bucket(&self.bucket);

                if let Some(continuation_token) = continuation_token {
                    builder = builder.continuation_token(continuation_token);
                }

                if let Some(prefix) = &self.prefix {
                    builder = builder.prefix(prefix);
                }

                let resp = builder.send().await?;

                for object_desc in resp.contents() {
                    if let Some(key) = object_desc.key() {
                        if BundleType::iter().any(|bundle_type| key.ends_with(bundle_type.suffix()))
                        {
                            let mut object_data = client
                                .get_object()
                                .bucket(&self.bucket)
                                .key(key)
                                .send()
                                .await?;

                            while let Some(bytes) = object_data.body.try_next().await? {
                                write.write_all(&bytes)?;
                            }

                            let bundle_name = key.split('/').last().unwrap_or(key).to_string();

                            if self.delete_after_load {
                                client
                                    .delete_object()
                                    .bucket(&self.bucket)
                                    .key(key)
                                    .send()
                                    .await?;
                            }

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

        anyhow::bail!("No bundles found in the bucket")
    }
}
