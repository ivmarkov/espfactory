use std::io::{self, Read, Seek};

use anyhow::Context;

use log::info;

use crate::uploader::log_name;

use super::BundleLogsUploader;

/// A logs uploader that uploads logs to an HTTP(S) server.
///
/// The server is expected to respond to POST requests with the logs data as follows:
/// - If the `id` argument is present when calling `upload_logs`, then a POST request with a parameter `id` is submitted to the server as follows:
///   `POST <path-from-url>?id=<id>`
/// - If the `id` argument is not present when calling `upload_logs`, then a POST request is submitted to the server as follows:
///   `POST <path-from-url>`
#[derive(Debug, Clone)]
pub struct HttpLogsUploader {
    logs_upload_url: String,
    name_as_log_file: bool,
    auth: Option<String>,
}

impl HttpLogsUploader {
    /// Creates a new `HttpLogsUploader`
    ///
    /// # Arguments
    /// - `logs_upload_url`: The URL of the server to upload the logs to
    /// - `auth`: An optional authorization token to use when uploading the logs
    /// - `name_as_log_file`: A flag indicating whether to upload the bundle logs with:
    ///   - `true`:  A simple parameter-less POST request of the form `<url>/<bundle-name>.log.zip`
    ///   - `false`: With a POST request with a parameter `<url>?id=<bundle-id>`
    pub const fn new(
        logs_upload_url: String,
        auth: Option<String>,
        name_as_log_file: bool,
    ) -> Self {
        Self {
            logs_upload_url,
            auth,
            name_as_log_file,
        }
    }
}

impl BundleLogsUploader for HttpLogsUploader {
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
                "About to upload logs `{log_name}` for Bundle ID `{bundle_id}` to URL `{}`...",
                self.logs_upload_url
            );
        } else {
            info!(
                "About to uploads logs `{log_name}` to URL `{}`...",
                self.logs_upload_url
            );
        }

        let client = reqwest::Client::new();

        let mut builder = if self.name_as_log_file {
            // When `name_as_log_file` is `true`, we dictate the name of the uploaded log file
            let url = format!("{}/{log_name}", self.logs_upload_url.trim_end_matches('/'));

            client.post(&url)
        } else if let Some(id) = bundle_id {
            client.post(&self.logs_upload_url).query(&[("id", id)])
        } else {
            client.post(&self.logs_upload_url)
        };

        builder = builder.header(
            "Content-Disposition",
            format!("attachment; filename=\"{log_name}\""),
        );

        if let Some(auth) = self.auth.as_deref() {
            builder = builder.header("Authorization", auth);
        }

        read.seek(io::SeekFrom::Start(0))
            .context("Saving the bundle log failed")?;

        // A bit of a hack as it reads the whole ZIP in memory
        let mut data = Vec::new();
        read.read_to_end(&mut data)?;

        builder
            .body(data)
            .send()
            .await
            .context("Request failed")?
            .error_for_status()
            .context("Request returned an error status")?;

        info!("Logs `{log_name}` uploaded");

        Ok(())
    }
}
