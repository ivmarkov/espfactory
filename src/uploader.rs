use std::io::{Read, Seek};

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
