use std::path::PathBuf;

use async_compat::CompatExt;

use clap::{ColorChoice, Parser, Subcommand, ValueEnum};

use espfactory::loader::{dir::DirLoader, http::HttpLoader, BundleLoader};
use espfactory::{BundleIdentification, Config, LOGGER};

use log::LevelFilter;

use serde::{Deserialize, Serialize};

extern crate alloc;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None, arg_required_else_help = true, color = ColorChoice::Auto)]
struct Cli {
    /// Verbosity
    #[arg(short = 'l', long, default_value = "verbose")]
    verbosity: Verbosity,

    /// Configuration file
    #[arg(short = 'c', long)]
    conf: Option<PathBuf>,

    /// Command
    #[command(subcommand)]
    command: Option<BundleSource>,
}

/// Command
#[derive(Subcommand, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum BundleSource {
    /// Load bundles from a directory
    Dir {
        #[arg(short = 'd', long)]
        delete_after_load: bool,

        load_path: PathBuf,

        logs_upload_path: Option<PathBuf>,
    },
    /// Load bundles from an HTTP(s) server
    Http {
        #[arg(short = 'a', long)]
        authorization: Option<String>,

        load_url: String,

        logs_upload_url: Option<String>,
    },
    /// Load bundles from an S3 bucket
    #[cfg(feature = "s3")]
    S3 {
        #[arg(short = 'd', long)]
        delete_after_load: bool,

        load_path: String,

        logs_upload_path: Option<String>,
    },
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
//#[cfg_attr(feature = "bin", )]
pub struct Settings {
    /// The source of bundles
    pub bundle_source: Option<BundleSource>,
    pub config: Config,
}

impl Settings {
    /// Create a new configuration with default values
    /// (no port, no bundle identification method, no readouts)
    pub const fn new() -> Self {
        Self {
            bundle_source: None,
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
    /// Load bundles from a directory
    Dir(DirLoader),
    /// Load bundles from an HTTP(s) server
    Http(HttpLoader),
    /// Load bundles from an S3 bucket
    #[cfg(feature = "s3")]
    S3(espfactory::loader::s3::S3Loader),
}

impl BundleLoader for Loader {
    async fn load<W>(&mut self, write: W, id: Option<&str>) -> anyhow::Result<String>
    where
        W: std::io::Write,
    {
        match self {
            Self::Dir(loader) => loader.load(write, id).await,
            Self::Http(loader) => loader.load(write, id).await,
            #[cfg(feature = "s3")]
            Self::S3(loader) => loader.load(write, id).await,
        }
    }

    async fn upload_logs<R>(&mut self, read: R, id: Option<&str>, name: &str) -> anyhow::Result<()>
    where
        R: std::io::Read,
    {
        match self {
            Self::Dir(loader) => loader.upload_logs(read, id, name).await,
            Self::Http(loader) => loader.upload_logs(read, id, name).await,
            #[cfg(feature = "s3")]
            Self::S3(loader) => loader.upload_logs(read, id, name).await,
        }
    }
}

fn main() -> anyhow::Result<()> {
    let args = Cli::parse();

    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(LevelFilter::Debug);

    let conf = if let Some(conf) = args.conf {
        toml::from_str(&std::fs::read_to_string(conf)?)?
    } else {
        Settings {
            bundle_source: None,
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

    let loader = args
        .command
        .or(conf.bundle_source.clone())
        .map(|command| match command {
            BundleSource::Dir {
                load_path: path,
                delete_after_load,
                logs_upload_path,
            } => Loader::Dir(DirLoader::new(path, delete_after_load, logs_upload_path)),
            BundleSource::Http {
                load_url,
                authorization,
                logs_upload_url,
            } => Loader::Http(HttpLoader::new(load_url, authorization, logs_upload_url)),
            #[cfg(feature = "s3")]
            BundleSource::S3 {
                load_path,
                delete_after_load,
                logs_upload_path,
            } => Loader::S3(espfactory::loader::s3::S3Loader::new_from_path(
                None,
                load_path,
                delete_after_load,
                logs_upload_path,
            )),
        });

    if let Some(loader) = loader {
        let project_dirs = directories::ProjectDirs::from("org", "ivmarkov", "espfactory")
            .ok_or_else(|| anyhow::anyhow!("Cannot mount project directories"))?;

        let bundle_dir = &project_dirs.cache_dir().join("bundle");

        LOGGER.lock(|logger| logger.set_level(args.verbosity.log_level()));

        std::env::set_var("RUST_LIB_BACKTRACE", "1");

        futures_lite::future::block_on(espfactory::run(&conf.config, bundle_dir, loader).compat())?;
    }

    Ok(())
}
