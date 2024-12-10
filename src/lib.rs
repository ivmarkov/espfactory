#![allow(async_fn_in_trait)]

use std::path::Path;

use alloc::sync::Arc;

use embassy_futures::select::select;

use input::Input;
use model::Model;
use task::Task;
use utils::futures::Coalesce;
use view::View;

pub use loader::*;

extern crate alloc;

mod bundle;
mod flash;
mod input;
mod loader;
mod model;
mod task;
mod utils;
mod view;

pub async fn run<T>(com_port: Option<String>, bundle_dir: &Path, loader: T) -> anyhow::Result<()>
where
    T: BundleLoader,
{
    let mut terminal = ratatui::init();

    let model = Arc::new(Model::new());

    let result = select(
        View::new(&model, &mut terminal).run(),
        Task::new(model.clone(), com_port.as_deref(), bundle_dir, loader)
            .run(&mut Input::new(&model)),
    )
    .coalesce()
    .await;

    ratatui::restore();

    result
}
