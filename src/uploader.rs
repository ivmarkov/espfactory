use std::{
    io::{Read, Seek},
    path::PathBuf,
};

use url::Url;

pub mod dir;
pub mod http;
#[cfg(feature = "s3")]
pub mod s3;

/// A trait that uploads a bundle processing logs to a location
pub trait BundleLogsUploader {
    async fn upload_logs<R>(
        &mut self,
        _read: R,
        _id: Option<&str>,
        _name: &str,
    ) -> anyhow::Result<()>
    where
        R: Read + Seek,
    {
        // Do nothing by default
        Ok(())
    }
}

impl<T> BundleLogsUploader for &mut T
where
    T: BundleLogsUploader,
{
    async fn upload_logs<R>(&mut self, read: R, id: Option<&str>, name: &str) -> anyhow::Result<()>
    where
        R: Read + Seek,
    {
        (*self).upload_logs(read, id, name).await
    }
}

/// Wrapper enum for the loaders supported OOTB
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum LogsUploader {
    /// Load bundles from a directory
    Dir(dir::DirLogsUploader),
    /// Load bundles from an HTTP(s) server
    Http(http::HttpLogsUploader),
    /// Load bundles from an S3 bucket
    #[cfg(feature = "s3")]
    S3(s3::S3LogsUploader),
}

impl LogsUploader {
    pub fn new(url: &Url) -> anyhow::Result<Self> {
        match url.scheme() {
            "dir" => Ok(Self::Dir(dir::DirLogsUploader::new(PathBuf::from(
                url.path().to_string(),
            )))),
            "http" | "https" => Ok(Self::Http(http::HttpLogsUploader::new(
                url.as_str().to_string(),
                None,
                true,
            ))),
            #[cfg(feature = "s3")]
            "s3" => {
                let bucket = url
                    .host_str()
                    .ok_or_else(|| anyhow::anyhow!("No bucket provided in URL: {}", url))?
                    .to_string();
                let path = url.path().trim_matches('/');
                let path = (!path.is_empty()).then(|| path.to_string());

                Ok(Self::S3(s3::S3LogsUploader::new(None, bucket, path)))
            }
            _ => anyhow::bail!("Unsupported logs upload URL: {url}"),
        }
    }
}

impl BundleLogsUploader for LogsUploader {
    async fn upload_logs<R>(&mut self, read: R, id: Option<&str>, name: &str) -> anyhow::Result<()>
    where
        R: std::io::Read + std::io::Seek,
    {
        match self {
            Self::Dir(loader) => loader.upload_logs(read, id, name).await,
            Self::Http(loader) => loader.upload_logs(read, id, name).await,
            #[cfg(feature = "s3")]
            Self::S3(loader) => loader.upload_logs(read, id, name).await,
        }
    }
}

/// A logs uploader that uploads the logs to multiple destinations
pub struct MultilogsUploader<'a, T>(pub &'a mut [T]);

impl<'a, T> BundleLogsUploader for MultilogsUploader<'a, T>
where
    T: BundleLogsUploader,
{
    async fn upload_logs<R>(
        &mut self,
        mut read: R,
        id: Option<&str>,
        name: &str,
    ) -> anyhow::Result<()>
    where
        R: std::io::Read + std::io::Seek,
    {
        for uploader in self.0.iter_mut() {
            if let Err(err) = uploader.upload_logs(&mut read, id, name).await {
                log::error!("Error when uploading logs: {err}");
            }
        }

        Ok(())
    }
}
