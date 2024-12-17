use core::cmp::Ordering;

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, Widget};
use ratatui::DefaultTerminal;

use crate::bundle::{Bundle, ProvisioningStatus};
use crate::logger::LOGGER;
use crate::model::{Model, Prepared, Preparing, Provisioning, Readouts, State, Status};

/// The view (UI) of the application
///
/// The UI is interactive, terminal based
pub struct View<'a, 'b> {
    /// The model of the application to be rendered in the UI
    model: &'a Model,
    /// The terminal to render the UI to
    term: &'b mut DefaultTerminal,
}

impl<'a, 'b> View<'a, 'b> {
    /// Creates a new `View` instance with the given model and terminal
    pub fn new(model: &'a Model, term: &'b mut DefaultTerminal) -> Self {
        Self { model, term }
    }

    /// Runs the view rendering loop by watching for changes in the model and re-rendering the UI
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
            State::Readouts(readouts) => readouts.render(area, buf),
            State::Preparing(searching) => searching.render(area, buf),
            State::PreparingFailed(failure) => failure.render(area, buf),
            State::Prepared(loaded) => loaded.render(area, buf),
            State::Provisioning(provisioning) => provisioning.render(area, buf),
            State::ProvisioningOutcome(outcome) => outcome.render(area, buf),
        }
    }
}

impl Widget for &Readouts {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = render_main(
            Some(" Readouts ".bold()),
            Some(Line::from(vec![
                "Readout ".into(),
                "<chars> + <Enter> ".blue().bold(),
                "Reset ".into(),
                "<Esc> ".blue().bold(),
            ])),
            area,
            buf,
        );

        let layout = Layout::new(
            Direction::Vertical,
            [Constraint::Min(5), Constraint::Length(100)],
        )
        .split(area.inner(Margin::new(2, 2)));

        Table::new(
            self.readouts
                .iter()
                .enumerate()
                .map(|(index, (name, value))| {
                    let mut row = Row::new::<Vec<Cell>>(vec![
                        if index == self.active { ">" } else { "" }.into(),
                        name.as_str().into(),
                        match self.active.cmp(&index) {
                            Ordering::Less => "(empty)".into(),
                            Ordering::Equal => format!("{}_", value.as_str()).into(),
                            Ordering::Greater => value.as_str().into(),
                        },
                    ]);

                    if index == self.active {
                        row = row.bold();
                    }

                    row
                })
                .collect::<Vec<_>>(),
            vec![
                Constraint::Length(1),
                Constraint::Percentage(20),
                Constraint::Percentage(80),
            ],
        )
        .header(Row::new::<Vec<Cell>>(vec!["".into(), "Name".into(), "Value".into()]).gray())
        .render(layout[0], buf);
    }
}

impl Widget for &Preparing {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = render_main(
            Some(Span::from(" Bundle Preparation ").bold()),
            Some(Line::from(vec![" Quit ".into(), "<Esc> ".blue().bold()])),
            area,
            buf,
        );

        const PROGRESS: &[char] = &['-', '\\', '|', '/'];

        let counter_text = Text::from(format!(
            "{}... {}",
            if self.status.is_empty() {
                "Looking for firmware bundles".into()
            } else {
                self.status.clone()
            },
            PROGRESS[self.counter.0 % 4]
        ))
        .bold();

        Paragraph::new(counter_text)
            .left_aligned()
            .render(area.inner(Margin::new(2, 2)), buf);
    }
}

impl Widget for &Prepared {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let pb = ProvisionedBundle {
            bundle: &self.bundle,
            provisioning: false,
        };

        pb.render(area, buf);
    }
}

impl Widget for &Provisioning {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let pb = ProvisionedBundle {
            bundle: &self.bundle,
            provisioning: true,
        };

        pb.render(area, buf);
    }
}

impl Widget for &Status {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = render_main(
            Some(self.title.clone().bold()),
            Some(if self.error {
                Line::from(vec![
                    " Re-try ".into(),
                    "<Enter> ".blue().bold(),
                    "Quit ".into(),
                    "<Esc> ".blue().bold(),
                ])
            } else {
                Line::from(vec![" Continue ".into(), "<Enter> ".blue().bold()])
            }),
            area,
            buf,
        );

        let mut para = Paragraph::new(self.message.clone()).bold();

        if self.error {
            para = para.red();
        } else {
            para = para.green();
        }

        para.render(area.inner(Margin::new(2, 4)), buf);
    }
}

#[derive(Debug)]
struct ProvisionedBundle<'a> {
    bundle: &'a Bundle,
    provisioning: bool,
}

impl ProvisionedBundle<'_> {
    fn mark_available(mut row: Row<'_>, status: Option<ProvisioningStatus>) -> Row<'_> {
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

    fn active_string(status: Option<ProvisioningStatus>) -> String {
        if status
            .map(|status| matches!(status, ProvisioningStatus::InProgress(_)))
            .unwrap_or(false)
        {
            ">"
        } else {
            ""
        }
        .into()
    }

    fn status_string(status: Option<ProvisioningStatus>) -> String {
        match status {
            Some(ProvisioningStatus::NotStarted) => "Not Started".into(),
            Some(ProvisioningStatus::Pending) => "Pending".into(),
            Some(ProvisioningStatus::InProgress(progress)) => format!("{}%", progress),
            Some(ProvisioningStatus::Done) => "Done".into(),
            None => "-".into(),
        }
    }
}

impl Widget for &ProvisionedBundle<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = render_main(
            Some(Line::from(vec![
                " ".into(),
                "Bundle ".bold(),
                self.bundle.name.as_str().bold(),
                " ".into(),
            ])),
            (!self.provisioning).then(|| {
                Line::from(vec![
                    " Provision ".into(),
                    "<Enter> ".blue().bold(),
                    "Quit ".into(),
                    "<Esc> ".blue().bold(),
                ])
            }),
            area,
            buf,
        );

        let layout = Layout::new(
            Direction::Vertical,
            [
                Constraint::Min(1),
                Constraint::Min((self.bundle.parts_mapping.len() + 1) as _),
                Constraint::Min(1),
                Constraint::Min(1),
                Constraint::Min(3),
                Constraint::Percentage(100),
            ],
        )
        .split(area.inner(Margin::new(2, 2)));

        Paragraph::new("== Partitions")
            .bold()
            .render(layout[0], buf);

        Table::new(
            self.bundle.parts_mapping.iter().map(|mapping| {
                let row = Row::new::<Vec<Cell>>(vec![
                    ProvisionedBundle::active_string(mapping.status()).into(),
                    mapping.partition.name().into(),
                    if matches!(
                        mapping.partition.name().as_str(),
                        Bundle::BOOTLOADER_NAME | Bundle::PART_TABLE_NAME
                    ) {
                        "".into()
                    } else {
                        mapping.partition.ty().to_string().into()
                    },
                    if matches!(
                        mapping.partition.name().as_str(),
                        Bundle::BOOTLOADER_NAME | Bundle::PART_TABLE_NAME
                    ) {
                        "".into()
                    } else {
                        mapping.partition.subtype().to_string().into()
                    },
                    Text::raw(format!("0x{:06x}", mapping.partition.offset()))
                        .right_aligned()
                        .into(),
                    Text::raw(format!(
                        "{}KB (0x{:06x})",
                        mapping.partition.size() / 1024
                            + if mapping.partition.size() % 1024 > 0 {
                                1
                            } else {
                                0
                            },
                        mapping.partition.size()
                    ))
                    .right_aligned()
                    .into(),
                    "-".into(),
                    Text::raw(
                        mapping
                            .image
                            .as_ref()
                            .map(|image| {
                                format!(
                                    "{}KB (0x{:06x})",
                                    image.data.len() / 1024
                                        + if image.data.len() % 1024 > 0 { 1 } else { 0 },
                                    image.data.len()
                                )
                            })
                            .unwrap_or("-".to_string()),
                    )
                    .right_aligned()
                    .into(),
                    Text::raw(ProvisionedBundle::status_string(mapping.status()))
                        .right_aligned()
                        .into(),
                ]);

                ProvisionedBundle::mark_available(row, mapping.status())
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
        .render(layout[1], buf);

        Paragraph::new("== EFUSE").bold().render(layout[3], buf);
    }
}

fn render_main<'a>(
    title: Option<impl Into<Line<'a>>>,
    instructions: Option<impl Into<Line<'a>>>,
    area: Rect,
    buf: &mut Buffer,
) -> Rect {
    let layout = Layout::vertical([Constraint::Percentage(100), Constraint::Length(4)]).split(area);

    main_block(title, instructions).render(layout[0], buf);
    render_log(layout[1], buf);

    layout[0]
}

fn render_log(area: Rect, buf: &mut Buffer) {
    for (index, line) in LOGGER.lock().last_n(area.height as usize).enumerate() {
        let line = Line::from(line.message.as_str());
        line.render(Rect::new(area.x, area.y + index as u16, area.width, 1), buf);
    }
}

fn main_block<'a, T, I>(title: Option<T>, instructions: Option<I>) -> Block<'a>
where
    T: Into<Line<'a>>,
    I: Into<Line<'a>>,
{
    let mut block = Block::bordered().title_top(
        Line::from(" ESP32 Factory Provisioning ")
            .bold()
            .left_aligned()
            .green(),
    );

    if let Some(title) = title {
        block = block.title_top(title.into().bold().centered().green());
    }

    if let Some(instructions) = instructions {
        block = block.title_bottom(instructions.into().right_aligned().yellow());
    }

    block.on_blue().white()
}
