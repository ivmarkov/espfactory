use std::io::Write;

use anyhow::Context;

use super::BundleLoader;

#[derive(Debug, Clone)]
pub struct HttpLoader {
    url: String,
    auth: Option<String>,
}

impl HttpLoader {
    pub const fn new(url: String, auth: Option<String>) -> Self {
        Self { url, auth }
    }
}

impl BundleLoader for HttpLoader {
    async fn load<W>(&mut self, mut write: W, id: Option<&str>) -> anyhow::Result<String>
    where
        W: Write,
    {
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
                .context("Writing the response failed")?;
        }

        Ok(bundle_name)
    }
}
