use std::io::{Read, Write};

use anyhow::Context;

use log::info;

use super::BundleLoader;

/// A loader that reads bundles from an HTTP(S) server.
///
/// The server is expected to respond to either GET or POST requests with the bundle data as follows:
/// - If the `id` argument is present when calling `load`, then a GET request with a parameter `id` is submitted to the server as follows:
///   `GET <path-from-url>?id=<id>`
///   The server should respond with the bundle data if a bundle with the supplied ID is found or with an HTTP error status code
///   otherwise; or with another HTTP error status code if the request failed due to other problems (invalid auth, server error, etc.)
/// - If the `id` argument is not present when calling `load`, then a POST request is submitted to the server as follows:
///   `POST <path-from-url>`
///   The server should respond with the bundle data if a (random/next) bundle is found or with an HTTP error status code otherwise; or with another
///   HTTP error status code if the request failed due to other problems (invalid auth, server error, etc.)
///   The server might also delete the provided bundle after it has been provided
///   
/// In both cases (bundle loading with or without a bundle ID), the server should provide the bundle data in the response body
/// and the name of the bundle in the `Content-Disposition` header. If the `Content-Disposition` header is not present, then the name of the bundle
/// is assumed to be the ID of the bundle with the `.bundle` extension, or a random name with the `.bundle` extension if the ID is not present
#[derive(Debug, Clone)]
pub struct HttpLoader {
    url: String,
    auth: Option<String>,
}

impl HttpLoader {
    /// Creates a new `HttpLoader`
    ///
    /// # Arguments
    /// - `url`: The URL of the server to load the bundles from
    /// - `auth`: An optional authorization token to use when loading the bundles
    ///           If present, it will be used as the value of the `Authorization` header
    ///           in the request
    pub const fn new(url: String, auth: Option<String>) -> Self {
        Self { url, auth }
    }
}

impl BundleLoader for HttpLoader {
    async fn load<W>(&mut self, mut write: W, id: Option<&str>) -> anyhow::Result<String>
    where
        W: Write,
    {
        if let Some(id) = id {
            info!(
                "About to fetch a bundle with ID `{id}` from URL `{}`...",
                self.url
            );
        } else {
            info!("About to fetch a random bundle from URL `{}`...", self.url);
        }

        let client = reqwest::Client::new();

        let mut builder = if let Some(id) = id {
            client.get(&self.url).query(&[("id", id)])
        } else {
            client.post(&self.url)
        };

        if let Some(auth) = self.auth.as_deref() {
            builder = builder.header("Authorization", auth);
        }

        let response = builder.send().await.context("Request failed")?;

        let mut response = response
            .error_for_status()
            .context("Request returned an error status")?;

        let mut bundle_name = format!("{}.bundle", id.unwrap_or("firmware"));

        if let Some(cont_disp) = response
            .headers()
            .get("Content-Disposition")
            .and_then(|value| value.to_str().ok())
        {
            let mut split = cont_disp.split(";");
            if let Some(name) = split.find_map(|part| part.trim().strip_prefix("filename=")) {
                bundle_name = name.trim().to_string();
            }
        }

        while let Some(bytes) = response
            .chunk()
            .await
            .context("Reading the response failed")?
        {
            write
                .write_all(&bytes)
                .context("Loading the bundle failed")?;
        }

        info!("Loaded bundle `{}`", bundle_name);

        Ok(bundle_name)
    }

    async fn upload_logs<R>(
        &mut self,
        _read: R,
        _id: Option<&str>,
        _name: &str,
    ) -> anyhow::Result<()>
    where
        R: Read,
    {
        // Do nothing by default
        Ok(())
    }
}
