use std::io::Write;

use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::get_object::GetObjectError;

use super::{BundleLoader, BundleType};

#[derive(Debug, Clone)]
pub struct S3Loader {
    bucket: String,
    prefix_key: Option<String>,
}

impl S3Loader {
    pub const fn new(bucket: String, prefix_key: Option<String>) -> Self {
        Self { bucket, prefix_key }
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
                    .prefix_key
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

                if let Some(prefix) = &self.prefix_key {
                    builder = builder.prefix(prefix);
                }

                let resp = builder.send().await?;

                // NOTE: Not really handling truncation
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
