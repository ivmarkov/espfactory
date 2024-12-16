use std::path::PathBuf;

use clap::{ColorChoice, Parser, Subcommand, ValueEnum};

use espfactory::loader::{dir::DirLoader, http::HttpLoader, s3::S3Loader, BundleLoader};
use espfactory::BundleIdentification;

use log::LevelFilter;

use serde::Deserialize;

extern crate alloc;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None, arg_required_else_help = true, color = ColorChoice::Auto)]
struct Cli {
    /// Verbosity
    #[arg(short = 'l', long, default_value = "regular")]
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
    #[default]
    Regular,
    /// Verbose logging
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
}

impl Config {
    /// Create a new configuration with default values
    /// (no port, no bundle identification method, no readouts)
    pub const fn new() -> Self {
        Self {
            port: None,
            bundle_source: None,
            bundle_identification: None,
            test_jig_id_readout: None,
            pcb_id_readout: None,
            box_id_readout: None,
        }
    }

    /// Convert the configuration to the library configuration format
    pub fn to_lib_config(&self) -> espfactory::Config {
        espfactory::Config {
            port: self.port.as_ref().cloned(),
            bundle_identification: self.bundle_identification.unwrap_or_default(),
            test_jig_id_readout: self.test_jig_id_readout.unwrap_or_default(),
            pcb_id_readout: self.pcb_id_readout.unwrap_or_default(),
            box_id_readout: self.box_id_readout.unwrap_or_default(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

/// Wrapper enum for the loaders supported OOTB
#[derive(Debug)]
pub enum Loader {
    /// Load bundles from a directory
    Dir(DirLoader),
    /// Load bundles from an HTTP(s) server
    Http(HttpLoader),
    /// Load bundles from an S3 bucket
    S3(S3Loader),
}

impl BundleLoader for Loader {
    async fn load<W>(&mut self, write: W, id: Option<&str>) -> anyhow::Result<String>
    where
        W: std::io::Write,
    {
        match self {
            Self::Dir(loader) => loader.load(write, id).await,
            Self::Http(loader) => loader.load(write, id).await,
            Self::S3(loader) => loader.load(write, id).await,
        }
    }
}

fn main() -> anyhow::Result<()> {
    let args = Cli::parse();

    // env_logger::builder()
    //     .format(|buf, record| writeln!(buf, "{}", record.args()))
    //     .filter_level(args.verbosity.log_level())
    //     .init();

    let conf = if let Some(conf) = args.conf {
        toml::from_str(&std::fs::read_to_string(conf)?)?
    } else {
        Config {
            port: None,
            bundle_source: None,
            bundle_identification: Some(BundleIdentification::BoxId),
            test_jig_id_readout: Some(true),
            pcb_id_readout: Some(true),
            box_id_readout: Some(true),
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
            BundleSource::S3 {
                path,
                delete_after_load,
            } => Loader::S3(S3Loader::new_from_path(path, delete_after_load)),
        });

    if let Some(loader) = loader {
        // if let Err(err) = result {
        //     log::error!("{:#}", err);
        //     std::process::exit(1);
        // }

        let project_dirs = directories::ProjectDirs::from("org", "ivmarkov", "espfactory")
            .ok_or_else(|| anyhow::anyhow!("Cannot mount project directories"))?;

        let bundle_dir = &project_dirs.cache_dir().join("bundle");

        futures_lite::future::block_on(espfactory::run(&conf.to_lib_config(), bundle_dir, loader))?;
    }

    Ok(())
}
