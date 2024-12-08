use std::path::PathBuf;

use embassy_futures::select::select;

use input::Input;
use loader::DirLoader;
use ratatui::DefaultTerminal;

use model::Model;
use task::Task;
use utils::futures::Coalesce;
use view::View;

mod bundle;
mod input;
mod loader;
mod model;
mod task;
mod utils;
mod view;

fn main() -> anyhow::Result<()> {
    let mut terminal = ratatui::init();

    let model = Model::new();

    let result = futures_lite::future::block_on(run(&model, &mut terminal));

    ratatui::restore();

    result
}

async fn run(model: &Model, terminal: &mut DefaultTerminal) -> anyhow::Result<()> {
    select(
        View::new(model, terminal).run(),
        // TODO
        Task::new(
            model,
            &PathBuf::from("scratch/bundles"),
            DirLoader::new(PathBuf::from("scratch/bundles")),
        )
        .run(&mut Input::new(model)),
    )
    .coalesce()
    .await
}
