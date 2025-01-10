use std::path::PathBuf;

use anyhow::Context;

use async_compat::CompatExt;

use clap::{ColorChoice, Parser, ValueEnum};

use espfactory::loader::Loader;
use espfactory::uploader::{LogsUploader, MultilogsUploader};
use espfactory::{self, LOGGER};

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
    /// Warn logging (warn level)
    Warn,
    /// Regular logging (info level)
    #[default]
    Regular,
    /// Verbose logging (debug level)
    Verbose,
}

impl Verbosity {
    fn log_level(&self) -> LevelFilter {
        match self {
            Self::Silent => LevelFilter::Off,
            Self::Warn => LevelFilter::Warn,
            Self::Regular => LevelFilter::Info,
            Self::Verbose => LevelFilter::Debug,
        }
    }
}

/// The configuration of the factory
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// The base URL of the factory
    pub base_url: Option<Url>,
    /// The source of bundles
    pub url: Option<Url>,
    /// The destinations where to upload logs
    #[serde(default)]
    pub logs_upload_urls: Vec<Url>,
    /// The configuration of the factory
    #[serde(default)]
    pub config: espfactory::Config,
}

impl Config {
    /// Create a new configuration with default values
    /// (no port, no bundle identification method, no readouts)
    pub const fn new() -> Self {
        Self {
            base_url: None,
            url: None,
            logs_upload_urls: Vec::new(),
            config: espfactory::Config::new(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
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

    log::set_max_level(LevelFilter::Debug);

    let conf = if let Some(conf) = args.conf {
        println!("Loading configuration from `{}`", conf.display());
        toml::from_str(&std::fs::read_to_string(conf)?).context("Invalid configuiration format")?
    } else if let Ok(current_exe) = std::env::current_exe() {
        let conf = current_exe.with_file_name("espfactory.toml");
        if conf.exists() && conf.is_file() {
            println!("Loading configuration from `{}`", conf.display());
            toml::from_str(&std::fs::read_to_string(conf)?)
                .context("Invalid configuiration format")?
        } else {
            println!("Using default configuration");
            Config::new()
        }
    } else {
        println!("Using default configuration");
        Config::new()
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

    log::set_logger(&LOGGER).unwrap();

    LOGGER.lock(|logger| logger.set_level(args.verbosity.log_level()));

    std::env::set_var("RUST_LIB_BACKTRACE", "1");

    futures_lite::future::block_on(
        espfactory::run(
            &conf.config,
            bundle_dir,
            base_loader,
            loader,
            MultilogsUploader(&mut logs_uploaders),
        )
        .compat(),
    )?;

    Ok(())
}
