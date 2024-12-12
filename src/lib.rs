#![allow(async_fn_in_trait)]

use std::path::Path;

use alloc::sync::Arc;

use embassy_futures::select::select;

use input::Input;
use model::Model;
use serde::Deserialize;
use task::Task;
use utils::futures::Coalesce;
use view::View;

extern crate alloc;

mod bundle;
mod flash;
mod input;
pub mod loader;
mod model;
mod task;
mod utils;
mod view;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Deserialize)]
pub enum BundleIdentification {
    None,
    PcbId,
    BoxId,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
pub struct Config {
    pub port: Option<String>,
    pub bundle_identification: BundleIdentification,
    pub test_jig_id_readout: bool,
    pub pcb_id_readout: bool,
    pub box_id_readout: bool,
}

impl Config {
    pub const fn new() -> Self {
        Self {
            port: None,
            bundle_identification: BundleIdentification::None,
            test_jig_id_readout: false,
            pcb_id_readout: false,
            box_id_readout: false,
        }
    }
}
impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn run<T>(conf: &Config, bundle_dir: &Path, loader: T) -> anyhow::Result<()>
where
    T: loader::BundleLoader,
{
    let mut terminal = ratatui::init();

    let model = Arc::new(Model::new());

    let result = select(
        View::new(&model, &mut terminal).run(),
        Task::new(model.clone(), conf, bundle_dir, loader).run(&mut Input::new(&model)),
    )
    .coalesce()
    .await;

    ratatui::restore();

    result
}
