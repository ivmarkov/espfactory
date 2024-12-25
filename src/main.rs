use std::path::PathBuf;

use async_compat::CompatExt;

use clap::{ColorChoice, Parser, ValueEnum};

use espfactory::loader::file::FileLoader;
use espfactory::loader::{dir::DirLoader, http::HttpLoader, BundleLoader};
use espfactory::uploader::{dir::DirLogsUploader, http::HttpLogsUploader, BundleLogsUploader};
use espfactory::{BundleIdentification, Config, LOGGER};

use log::LevelFilter;

use serde::{Deserialize, Serialize};

use url::Url;

extern crate alloc;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None, color = ColorChoice::Auto)]
struct Cli {
    /// Verbosity
    #[arg(short = 'l', long, default_value = "verbose")]
    verbosity: Verbosity,

    /// Configuration file
    #[arg(short = 'c', long)]
    conf: Option<PathBuf>,

    /// Base bundle URL - the URL where the factory will look for a base bundle to load.
    /// Supported URL schemes:
    /// `file:` - load a base bundle from a file;
    /// `dir:` - load a base bundle from a directory;
    /// `http:` or `https:` - load a base bundle from an HTTP(s) server;
    /// `s3:` - load a base bundle from an S3 bucket
    #[arg(short = 'b', long)]
    base_url: Option<Url>,

    /// Bundle URL - the URL where the factory will look for a bundle to load.
    /// Supported URL schemes:
    /// `file:` - load a bundle from a file;
    /// `dir:` or `dird:` - load bundles from a directory; if `dird:` is used, the bundle will be removed after loading;
    /// `http:` or `https:` - load bundles from an HTTP(s) server;
    /// `s3:` or `s3d:` - load bundles from an S3 bucket; if `s3d:` is used, the bundle will be removed after loading
    url: Option<Url>,

    /// Logs upload URLs - the URLs where the factory will upload the logs from the device provisioning.
    /// Supported URL schemes:
    /// `dir:` - upload logs to a directory;
    /// `http:` or `https:` - upload logs to an HTTP(s) server;
    /// `s3:` - upload logs to an S3 bucket
    logs_urls: Vec<Url>,
}

/// Verbosity
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum Verbosity {
    /// Silent (no logging)
    Silent,
    /// Regular logging
    Regular,
    /// Verbose logging
    #[default]
    Verbose,
}

impl Verbosity {
    fn log_level(&self) -> LevelFilter {
        match self {
            Self::Silent => LevelFilter::Off,
            Self::Regular => LevelFilter::Info,
            Self::Verbose => LevelFilter::Debug,
        }
    }
}

/// The configuration of the factory
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// The base URL of the factory
    pub base_url: Option<Url>,
    /// The source of bundles
    pub url: Option<Url>,
    /// The destinations where to upload logs
    pub logs_upload_urls: Vec<Url>,
    /// The configuration of the factory
    pub config: Config,
}

impl Settings {
    /// Create a new configuration with default values
    /// (no port, no bundle identification method, no readouts)
    pub const fn new() -> Self {
        Self {
            base_url: None,
            url: None,
            logs_upload_urls: Vec::new(),
            config: Config::new(),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::new()
    }
}

/// Wrapper enum for the loaders supported OOTB
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Loader {
    /// Load a bundle from a file
    File(FileLoader),
    /// Load bundles from a directory
    Dir(DirLoader),
    /// Load bundles from an HTTP(s) server
    Http(HttpLoader),
    /// Load bundles from an S3 bucket
    #[cfg(feature = "s3")]
    S3(espfactory::loader::s3::S3Loader),
}

impl Loader {
    pub fn new(url: &Url, delete_after_load_allowed: bool) -> anyhow::Result<Self> {
        match url.scheme() {
            "file" => Ok(Self::File(FileLoader::new(PathBuf::from(
                url.path().to_string(),
            )))),
            "dir" | "dird" if delete_after_load_allowed => Ok(Self::Dir(DirLoader::new(
                PathBuf::from(url.path().to_string()),
                matches!(url.scheme(), "dird"),
                None,
            ))),
            "http" | "https" => Ok(Self::Http(HttpLoader::new(
                url.as_str().to_string(),
                None,
                None,
            ))),
            #[cfg(feature = "s3")]
            "s3" | "s3d" if delete_after_load_allowed => {
                let bucket = url
                    .host_str()
                    .ok_or_else(|| anyhow::anyhow!("No bucket provided in URL: {}", url))?
                    .to_string();
                let path = url.path().trim_matches('/');
                let path = (!path.is_empty()).then(|| path.to_string());

                Ok(Self::S3(espfactory::loader::s3::S3Loader::new(
                    None,
                    bucket,
                    path,
                    matches!(url.scheme(), "s3d"),
                    None,
                    None,
                )))
            }
            _ => anyhow::bail!("Unsupported bundle load URL: {url}"),
        }
    }
}

impl BundleLoader for Loader {
    async fn load<W>(&mut self, write: W, id: Option<&str>) -> anyhow::Result<String>
    where
        W: std::io::Write,
    {
        match self {
            Self::File(loader) => loader.load(write, id).await,
            Self::Dir(loader) => loader.load(write, id).await,
            Self::Http(loader) => loader.load(write, id).await,
            #[cfg(feature = "s3")]
            Self::S3(loader) => loader.load(write, id).await,
        }
    }
}

/// Wrapper enum for the loaders supported OOTB
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum LogsUploader {
    /// Load bundles from a directory
    Dir(DirLogsUploader),
    /// Load bundles from an HTTP(s) server
    Http(HttpLogsUploader),
    /// Load bundles from an S3 bucket
    #[cfg(feature = "s3")]
    S3(espfactory::uploader::s3::S3LogsUploader),
}

impl LogsUploader {
    pub fn new(url: &Url) -> anyhow::Result<Self> {
        match url.scheme() {
            "dir" => Ok(Self::Dir(DirLogsUploader::new(PathBuf::from(
                url.path().to_string(),
            )))),
            "http" | "https" => Ok(Self::Http(HttpLogsUploader::new(
                url.as_str().to_string(),
                None,
            ))),
            #[cfg(feature = "s3")]
            "s3" => {
                let bucket = url
                    .host_str()
                    .ok_or_else(|| anyhow::anyhow!("No bucket provided in URL: {}", url))?
                    .to_string();
                let path = url.path().trim_matches('/');
                let path = (!path.is_empty()).then(|| path.to_string());

                Ok(Self::S3(espfactory::uploader::s3::S3LogsUploader::new(
                    None, bucket, path,
                )))
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

struct Multi<'a, T>(&'a mut [T]);

impl<'a, T> BundleLogsUploader for Multi<'a, T>
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

fn main() {
    if let Err(err) = run() {
        eprintln!("Error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let args = Cli::parse();

    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(LevelFilter::Debug);

    let conf = if let Some(conf) = args.conf {
        toml::from_str(&std::fs::read_to_string(conf)?)?
    } else {
        Settings {
            base_url: None,
            url: None,
            logs_upload_urls: Vec::new(),
            config: Config {
                dry_run: true,
                bundle_identification: BundleIdentification::BoxId,
                test_jig_id_readout: true,
                pcb_id_readout: true,
                box_id_readout: true,
                ..Default::default()
            },
        }
    };

    let base_loader_url = args.base_url.or_else(|| conf.base_url.clone());

    let base_loader = base_loader_url
        .as_ref()
        .map(|url| Loader::new(url, false))
        .transpose()?;

    let loader_url = args.url.or_else(|| conf.url.clone());
    let Some(loader_url) = loader_url else {
        anyhow::bail!("No bundle URL provided");
    };

    let loader = Loader::new(&loader_url, true)?;

    let mut logs_upload_urls = args.logs_urls;

    if logs_upload_urls.is_empty() {
        logs_upload_urls = conf.logs_upload_urls.clone();
    }

    if logs_upload_urls.is_empty() {
        anyhow::bail!("No logs upload URLs provided");
    }

    let mut logs_uploaders = logs_upload_urls
        .iter()
        .map(LogsUploader::new)
        .collect::<anyhow::Result<Vec<_>>>()?;

    let project_dirs = directories::ProjectDirs::from("org", "ivmarkov", "espfactory")
        .ok_or_else(|| anyhow::anyhow!("Cannot mount project directories"))?;

    let bundle_dir = &project_dirs.cache_dir().join("bundle");

    LOGGER.lock(|logger| logger.set_level(args.verbosity.log_level()));

    std::env::set_var("RUST_LIB_BACKTRACE", "1");

    futures_lite::future::block_on(
        espfactory::run(
            &conf.config,
            bundle_dir,
            base_loader,
            loader,
            Multi(&mut logs_uploaders),
        )
        .compat(),
    )?;

    Ok(())
}
