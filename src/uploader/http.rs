use std::io::{self, Read, Seek};

use anyhow::Context;

use log::info;

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
        id: Option<&str>,
        name: &str,
    ) -> anyhow::Result<()>
    where
        R: Read + Seek,
    {
        if let Some(id) = id {
            info!(
                "About to upload logs `{name}.log.zip` for ID `{id}` to URL `{}`...",
                self.logs_upload_url
            );
        } else {
            info!(
                "About to uploads logs `{name}` to URL `{}`...",
                self.logs_upload_url
            );
        }

        let client = reqwest::Client::new();

        let mut builder = if self.name_as_log_file {
            // When `name_as_log_file` is `true`, we dictate the name of the uploaded log file
            let url = format!(
                "{}/{name}.log.zip",
                self.logs_upload_url.trim_end_matches('/')
            );

            client.post(&url)
        } else if let Some(id) = id {
            client.post(&self.logs_upload_url).query(&[("id", id)])
        } else {
            client.post(&self.logs_upload_url)
        };

        builder = builder.header(
            "Content-Disposition",
            format!("attachment; filename=\"{name}.log.zip\""),
        );

        if let Some(auth) = self.auth.as_deref() {
            builder = builder.header("Authorization", auth);
        }

        read.seek(io::SeekFrom::Start(0))
            .context("Saving the bundle log failed")?;

        // TODO
        //builder = builder.body(Body::new(read));

        builder
            .send()
            .await
            .context("Request failed")?
            .error_for_status()
            .context("Request returned an error status")?;

        info!("Logs `{name}.log.zip` uploaded");

        Ok(())
    }
}
