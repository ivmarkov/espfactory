use std::path::PathBuf;

use async_compat::CompatExt;

use clap::{ColorChoice, Parser, Subcommand, ValueEnum};

use espfactory::loader::{dir::DirLoader, http::HttpLoader, BundleLoader};
use espfactory::{BundleIdentification, LOGGER};

use log::LevelFilter;

use serde::Deserialize;

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
#[derive(Subcommand, Debug, Clone, Eq, PartialEq, Deserialize)]
pub enum BundleSource {
    /// Load bundles from a directory
    Dir {
        #[arg(short = 'd', long)]
        delete_after_load: bool,

        path: PathBuf,
    },
    /// Load bundles from an HTTP(s) server
    Http {
        #[arg(short = 'a', long)]
        authorization: Option<String>,

        uri: String,
    },
    /// Load bundles from an S3 bucket
    #[cfg(feature = "s3")]
    S3 {
        #[arg(short = 'd', long)]
        delete_after_load: bool,

        path: String,
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
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
//#[cfg_attr(feature = "bin", )]
pub struct Config {
    /// The serial port to use for communication with the device
    ///
    /// If not provided, the first available port where an ESP chip is
    /// detected will be used
    pub port: Option<String>,
    /// The flash speed to use for flashing the device
    ///
    /// If not provided, the default speed will be used
    pub flash_speed: Option<u32>,
    /// The source of bundles
    pub bundle_source: Option<BundleSource>,
    /// The method used to identify the bundle to be loaded
    ///
    /// If not provided, the first bundle found will be loaded
    pub bundle_identification: Option<BundleIdentification>,
    /// Whether to render a UI for reading the test jig ID
    ///
    /// The test jig Id is only read and used for logging purposes
    ///
    /// If not provided, `false` is assumed
    pub test_jig_id_readout: Option<bool>,
    /// Whether to render a UI for reading the PCB ID
    ///
    /// The PCB ID is used for logging purposes, but also and if the `BundleIdentification::PcbId` is used
    /// it is used to identify the bundle to be loaded
    ///
    /// If not provided, `false` is assumed
    pub pcb_id_readout: Option<bool>,
    /// Whether to render a UI for reading the box ID
    ///
    /// The box ID is used for logging purposes, but also and if the `BundleIdentification::BoxId` is used
    /// it is used to identify the bundle to be loaded
    ///
    /// If not provided, `false` is assumed
    pub box_id_readout: Option<bool>,
    /// Whether to skip confirmations
    ///
    /// If not provided, `false` is assumed
    pub skip_confirmations: Option<bool>,
}

impl Config {
    /// Create a new configuration with default values
    /// (no port, no bundle identification method, no readouts)
    pub const fn new() -> Self {
        Self {
            port: None,
            flash_speed: None,
            bundle_source: None,
            bundle_identification: None,
            test_jig_id_readout: None,
            pcb_id_readout: None,
            box_id_readout: None,
            skip_confirmations: None,
        }
    }

    /// Convert the configuration to the library configuration format
    pub fn to_lib_config(&self) -> espfactory::Config {
        espfactory::Config {
            port: self.port.as_ref().cloned(),
            flash_speed: self.flash_speed,
            bundle_identification: self.bundle_identification.unwrap_or_default(),
            test_jig_id_readout: self.test_jig_id_readout.unwrap_or_default(),
            pcb_id_readout: self.pcb_id_readout.unwrap_or_default(),
            box_id_readout: self.box_id_readout.unwrap_or_default(),
            skip_confirmations: self.skip_confirmations.unwrap_or_default(),
        }
    }
}

impl Default for Config {
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
}

fn main() -> anyhow::Result<()> {
    let args = Cli::parse();

    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(LevelFilter::Debug);

    let conf = if let Some(conf) = args.conf {
        toml::from_str(&std::fs::read_to_string(conf)?)?
    } else {
        Config {
            port: None,
            flash_speed: None,
            bundle_source: None,
            bundle_identification: Some(BundleIdentification::BoxId),
            test_jig_id_readout: Some(true),
            pcb_id_readout: Some(true),
            box_id_readout: Some(true),
            skip_confirmations: Some(false),
        }
    };

    let loader = args
        .command
        .or(conf.bundle_source.clone())
        .map(|command| match command {
            BundleSource::Dir {
                path,
                delete_after_load,
            } => Loader::Dir(DirLoader::new(path, delete_after_load)),
            BundleSource::Http { uri, authorization } => {
                Loader::Http(HttpLoader::new(uri, authorization))
            }
            #[cfg(feature = "s3")]
            BundleSource::S3 {
                path,
                delete_after_load,
            } => Loader::S3(espfactory::loader::s3::S3Loader::new_from_path(
                None,
                path,
                delete_after_load,
            )),
        });

    if let Some(loader) = loader {
        let project_dirs = directories::ProjectDirs::from("org", "ivmarkov", "espfactory")
            .ok_or_else(|| anyhow::anyhow!("Cannot mount project directories"))?;

        let bundle_dir = &project_dirs.cache_dir().join("bundle");

        LOGGER.lock(|logger| logger.set_level(args.verbosity.log_level()));

        std::env::set_var("RUST_LIB_BACKTRACE", "1");

        futures_lite::future::block_on(
            espfactory::run(&conf.to_lib_config(), bundle_dir, loader).compat(),
        )?;
    }

    Ok(())
}
