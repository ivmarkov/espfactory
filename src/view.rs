use std::collections::HashMap;

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, Widget};
use ratatui::DefaultTerminal;

use crate::bundle::{Bundle, Partition};
use crate::model::{
    Empty, Model, Prepared, Preparing, Provisioned, Provisioning, ProvisioningStatus, State,
};

pub struct View<'a, 'b> {
    model: &'a Model,
    term: &'b mut DefaultTerminal,
}

impl<'a, 'b> View<'a, 'b> {
    pub fn new(model: &'a Model, term: &'b mut DefaultTerminal) -> Self {
        Self { model, term }
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            self.model.get(|state| {
                self.term
                    .draw(|frame| frame.render_widget(state, frame.area()))
            })?;

            self.model.wait_changed().await;
        }
    }
}

impl Widget for &State {
    fn render(self, area: Rect, buf: &mut Buffer) {
        match self {
            State::Preparing(searching) => searching.render(area, buf),
            State::Empty(empty) => empty.render(area, buf),
            State::Prepared(loaded) => loaded.render(area, buf),
            State::Provisioning(provisioning) => provisioning.render(area, buf),
            State::Provisioned(ready) => ready.render(area, buf),
        }
    }
}

impl Widget for &Preparing {
    fn render(self, area: Rect, buf: &mut Buffer) {
        const PROGRESS: &[char] = &['-', '\\', '|', '/'];

        let instructions = Line::from(vec![" Quit ".into(), "<Q> ".blue().bold()]);

        let counter_text = Text::from(vec![Line::from(vec![
            if self.status.is_empty() {
                "Looking for firmware bundles".into()
            } else {
                self.status.clone().into()
            },
            "... ".into(),
            format!(" {} ", PROGRESS[self.counter.0 % 4]).into(),
        ])]);

        Paragraph::new(counter_text)
            .centered()
            .block(main_block(instructions))
            .render(area, buf);
    }
}

impl Widget for &Empty {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // TODO
    }
}

impl Widget for &Prepared {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let pb = ProvisionedBundle {
            bundle: &self.bundle,
            provisioning: false,
            efuses_status: None,
        };

        pb.render(area, buf);
    }
}

impl Widget for &Provisioning {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let pb = ProvisionedBundle {
            bundle: &self.bundle,
            provisioning: true,
            efuses_status: Some(&self.efuses_status),
        };

        pb.render(area, buf);
    }
}

impl Widget for &Provisioned {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // TODO
    }
}

struct ProvisionedBundle<'a> {
    bundle: &'a Bundle,
    provisioning: bool,
    efuses_status: Option<&'a HashMap<String, ProvisioningStatus>>,
}

impl<'a> ProvisionedBundle<'a> {
    fn mark_available<'r>(mut row: Row<'r>, status: Option<&ProvisioningStatus>) -> Row<'r> {
        if let Some(status) = status {
            row = row.bold();

            row = match status {
                ProvisioningStatus::NotStarted | ProvisioningStatus::Pending => row.white(),
                ProvisioningStatus::InProgress(_) => row.yellow(),
                ProvisioningStatus::Done => row.green(),
            };
        } else {
            row = row.italic().black();
        }

        row
    }

    fn active_string(status: Option<&ProvisioningStatus>) -> String {
        if status
            .as_ref()
            .map(|status| matches!(status, ProvisioningStatus::InProgress(_)))
            .unwrap_or(false)
        {
            ">"
        } else {
            ""
        }
        .into()
    }

    fn status_string(status: Option<&ProvisioningStatus>) -> String {
        match status {
            Some(ProvisioningStatus::NotStarted) => "Not Started".into(),
            Some(ProvisioningStatus::Pending) => "Pending".into(),
            Some(ProvisioningStatus::InProgress(progress)) => format!("{}%", *progress).into(),
            Some(ProvisioningStatus::Done) => "Done".into(),
            None => "-".into(),
        }
    }
}

impl Widget for &ProvisionedBundle<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut instructions = vec![];

        if !self.provisioning {
            instructions.extend_from_slice(&[
                " Provision ".into(),
                "<Enter> ".bold(),
                "Quit ".into(),
                "<Q> ".bold(),
            ]);
        }

        main_block(Line::from(instructions)).render(area, buf);

        let layout = Layout::new(
            Direction::Vertical,
            [
                Constraint::Min(1),
                Constraint::Min(1),
                Constraint::Min((self.bundle.partitions.len() + 1) as _),
                Constraint::Min(1),
                Constraint::Min(1),
                Constraint::Min(3),
                Constraint::Min(1),
                Constraint::Min(1),
                Constraint::Length(100),
            ],
        )
        .split(area.inner(Margin::new(2, 2)));

        Paragraph::new(Line::from(vec![
            "== Bundle ".into(),
            self.bundle.name.as_str().into(),
            " ==".into(),
        ]))
        .bold()
        .green()
        .centered()
        .render(layout[0], buf);

        Paragraph::new("== Partitions")
            .bold()
            .render(layout[1], buf);

        Table::new(
            self.bundle.partitions.iter().map(|partition| {
                let row = Row::new::<Vec<Cell>>(vec![
                    ProvisionedBundle::active_string(
                        partition.image.as_ref().map(|image| &image.status),
                    )
                    .into(),
                    partition.name.clone().into(),
                    partition.part_type.as_str().to_string().into(),
                    partition.part_subtype.clone().into(),
                    Text::raw(partition.offset_string()).right_aligned().into(),
                    Text::raw(partition.size_string()).right_aligned().into(),
                    "-".into(),
                    Text::raw(
                        partition
                            .image
                            .as_ref()
                            .map(|image| Partition::any_size_string(image.data.len()))
                            .unwrap_or("-".to_string()),
                    )
                    .right_aligned()
                    .into(),
                    ProvisionedBundle::status_string(
                        partition.image.as_ref().map(|image| &image.status),
                    )
                    .into(),
                ]);

                ProvisionedBundle::mark_available(
                    row,
                    partition.image.as_ref().map(|image| &image.status),
                )
            }),
            vec![
                Constraint::Length(1),
                Constraint::Length(15),
                Constraint::Length(10),
                Constraint::Length(10),
                Constraint::Length(10),
                Constraint::Length(17),
                Constraint::Length(15),
                Constraint::Length(17),
                Constraint::Length(11),
            ],
        )
        .header(
            Row::new::<Vec<Cell>>(vec![
                "".into(),
                "Name".into(),
                "Type".into(),
                "Subtype".into(),
                Text::raw("Offset").right_aligned().into(),
                Text::raw("Size").right_aligned().into(),
                "Flags".into(),
                Text::raw("Image").right_aligned().into(),
                Text::raw("Provision").right_aligned().into(),
            ])
            .gray(),
        )
        .render(layout[2], buf);

        Paragraph::new("== EFUSE").bold().render(layout[4], buf);

        Paragraph::new("== Log").bold().render(layout[7], buf);

        Paragraph::new("TBD").render(layout[8], buf);
    }
}

fn main_block(instructions: Line) -> Block {
    let title = Line::from(" ESP32 Factory Provisioning ").bold();

    Block::bordered()
        .title(title.left_aligned().green())
        .title_bottom(instructions.right_aligned().yellow())
        .on_blue()
        .white()
}
