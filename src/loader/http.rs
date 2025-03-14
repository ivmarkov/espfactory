use std::io::Write;

use anyhow::Context;

use log::info;

use super::BundleLoader;

/// A loader that reads bundles from an HTTP(S) server.
///
/// The server is expected to respond to either GET or POST requests with the bundle data as follows:
/// - If the `id` argument is present when calling `load`, then a GET/POST request with a parameter `id` is submitted to the server as follows:
///   `GET/POST <path-from-url>?id=<id>`
///   The server should respond with the bundle data if a bundle with the supplied ID is found or with an HTTP error status code
///   otherwise; or with another HTTP error status code if the request failed due to other problems (invalid auth, server error, etc.)
/// - If the `id` argument is not present when calling `load`, then a GET/POST request is submitted to the server as follows:
///   `GET/POST <path-from-url>`
///   The server should respond with the bundle data if a bundle is found or with an HTTP error status code otherwise; or with another
///   HTTP error status code if the request failed due to other problems (invalid auth, server error, etc.)
///   The server might also delete the provided bundle after it had been provided, in case the used request method is POST
///
/// In both cases (bundle loading with or without a bundle ID), the server should provide the bundle data in the response body
/// and the name of the bundle in the `Content-Disposition` header. If the `Content-Disposition` header is not present, then the name of the bundle
/// is assumed to be the ID of the bundle with the `.bundle` extension, or a random name with the `.bundle` extension if the ID is not present
#[derive(Debug, Clone)]
pub struct HttpLoader {
    load_url: String,
    auth: Option<String>,
    use_post: bool,
    id_as_bundle_file: bool,
    #[allow(unused)]
    logs_url: Option<String>,
}

impl HttpLoader {
    /// Creates a new `HttpLoader`
    ///
    /// # Arguments
    /// - `load_url`: The URL of the server to load the bundles from
    /// - `auth`: An optional authorization token to use when loading the bundles
    ///   If present, it will be used as the value of the `Authorization` header
    ///   in the request
    /// - `use_post`: A flag indicating whether to fetch the bundle with a GET or a POST request
    /// - `id_as_bundle_file`: A flag indicating whether to fetch the bundle with:
    ///   - `true`:  A simple parameter-less GET/POST request of the form `<url>/<bundle-id>.bundle`
    ///   - `false`: With a GET/POST request with a parameter `<url>?id=<bundle-id>`
    /// - `logs_url`: An optional URL of the server to check for uploaded logs;
    ///   if provided, the loader will only download a bundle if its logs are not yet uploaded, this preventing
    ///   flashing a bundle multiple times
    pub const fn new(
        load_url: String,
        auth: Option<String>,
        use_post: bool,
        id_as_bundle_file: bool,
        logs_url: Option<String>,
    ) -> Self {
        Self {
            load_url,
            auth,
            use_post,
            id_as_bundle_file,
            logs_url,
        }
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
                self.load_url
            );
        } else {
            info!("About to fetch a bundle from URL `{}`...", self.load_url);
        }

        let client = reqwest::Client::new();

        let mut builder = if let Some(id) = id {
            if self.id_as_bundle_file {
                // When `id_as_bundle_file` is `true`, we only fetch `.bundle` bundles for now (though we can try out .bin and elf too)
                let url = format!("{}/{id}.bundle", self.load_url.trim_end_matches('/'));

                if self.use_post {
                    client.post(&url)
                } else {
                    client.get(&url)
                }
            } else if self.use_post {
                client.post(&self.load_url).query(&[("id", id)])
            } else {
                client.get(&self.load_url).query(&[("id", id)])
            }
        } else if self.use_post {
            client.post(&self.load_url)
        } else {
            client.get(&self.load_url)
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
}
