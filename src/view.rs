use core::cmp::Ordering;

use bitflags::bitflags;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, Widget};
use ratatui::DefaultTerminal;

use crate::bundle::{Bundle, ProvisioningStatus};
use crate::logger::LOGGER;
use crate::model::{Model, Processing, Provision, Readout, State, Status};

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
            self.model.access(|state| {
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
            State::Readout(readouts) => readouts.render(area, buf),
            State::Provision(loaded) => loaded.render(area, buf),
            State::Processing(processing) => processing.render(area, buf),
            State::Status(status) => status.render(area, buf),
        }
    }
}

impl Widget for &Readout {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = render_main(
            Some(" Readouts ".bold()),
            Keys::INPUT | Keys::RESET | Keys::QUIT,
            area,
            buf,
        );

        let layout = Layout::new(
            Direction::Vertical,
            [
                Constraint::Min(1),
                Constraint::Min((self.efuse_readouts.len() + 1) as _),
                Constraint::Min(1),
                Constraint::Min(1),
                Constraint::Min((self.readouts.len() + 1) as _),
                Constraint::Percentage(100),
            ],
        )
        .split(area.inner(Margin::new(2, 2)));

        Paragraph::new("== eFuse Readouts")
            .bold()
            .render(layout[0], buf);

        Table::new(
            self.efuse_readouts
                .iter()
                .map(|(name, value)| {
                    Row::new::<Vec<Cell>>(vec![
                        "".into(),
                        name.as_str().into(),
                        value.as_str().into(),
                    ])
                })
                .collect::<Vec<_>>(),
            vec![
                Constraint::Length(1),
                Constraint::Percentage(20),
                Constraint::Percentage(80),
            ],
        )
        .header(Row::new::<Vec<Cell>>(vec!["".into(), "Name".into(), "Value".into()]).gray())
        .render(layout[1], buf);

        Paragraph::new("== Input Readouts")
            .bold()
            .render(layout[3], buf);

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
        .render(layout[4], buf);
    }
}

impl Provision {
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

impl Widget for &Provision {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = render_main(
            Some(Line::from(vec![
                " ".into(),
                "Bundle ".bold(),
                self.bundle.name.as_str().bold(),
                " ".into(),
            ])),
            Keys::CONFIRM | Keys::BACK | Keys::QUIT,
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
                    Provision::active_string(mapping.status()).into(),
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
                    Text::raw(Provision::status_string(mapping.status()))
                        .right_aligned()
                        .into(),
                ]);

                Provision::mark_available(row, mapping.status())
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

impl Widget for &Processing {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = render_main(
            Some(self.title.clone().bold()),
            Keys::BACK | Keys::QUIT,
            area,
            buf,
        );

        const PROGRESS: &[char] = &['-', '\\', '|', '/'];

        let counter_text = Text::from(format!(
            "{}... {}",
            if self.status.is_empty() {
                "Preparing".into()
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

impl Widget for &Status {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = render_main(
            Some(self.title.clone().bold()),
            if self.error {
                Keys::RETRY | Keys::BACK | Keys::QUIT
            } else {
                Keys::CONFIRM | Keys::QUIT
            },
            area,
            buf,
        );

        let mut para = Paragraph::new(self.message.clone()).bold();

        if self.error {
            para = para.yellow();
        }

        para.render(area.inner(Margin::new(2, 4)), buf);
    }
}

fn render_main<'a>(
    title: Option<impl Into<Line<'a>>>,
    keys: Keys,
    area: Rect,
    buf: &mut Buffer,
) -> Rect {
    let layout = Layout::vertical([Constraint::Percentage(100), Constraint::Length(6)]).split(area);

    let mut block = Block::bordered().title_top(
        Line::from(" ESP32 Factory Provisioning ")
            .bold()
            .left_aligned()
            .green(),
    );

    if let Some(title) = title {
        block = block.title_top(title.into().bold().centered().green());
    }

    if let Some(instructions) = keys.instructions() {
        block = block.title_bottom(instructions.right_aligned().yellow());
    }

    block.on_blue().white().render(layout[0], buf);

    let area = layout[1];

    let lines = LOGGER.lock(|logger| {
        logger
            .last_n(area.height as usize)
            .cloned()
            .collect::<Vec<_>>()
    });

    for (index, line) in lines.iter().enumerate() {
        let level = Span::from(format!("[{}] ", line.level.as_str()));

        let level = match line.level {
            log::Level::Error => level.red().bold(),
            log::Level::Warn => level.yellow().bold(),
            log::Level::Info => level.green(),
            log::Level::Debug => level.blue(),
            log::Level::Trace => level.cyan(),
        };

        let line = Line::from(vec![level, line.message.as_str().into()]);
        line.render(Rect::new(area.x, area.y + index as u16, area.width, 1), buf);
    }

    layout[0]
}

bitflags! {
    struct Keys: u8 {
        const QUIT = 0b00000;
        const RETRY = 0b00001;
        const CONFIRM = 0b00010;
        const BACK = 0b00100;
        const RESET = 0b01000;
        const INPUT = 0b10000;
    }
}

impl Keys {
    /// Render the instructions for the keys to be displayed
    fn instructions(&self) -> Option<Line<'static>> {
        (!self.is_empty()).then(|| {
            let mut instructions = Vec::new();

            if self.contains(Self::INPUT) {
                instructions.push(" Readout ".into());
                instructions.push("<chars> + <Enter>".yellow().bold());
            }

            if self.contains(Self::CONFIRM) {
                instructions.push(" Continue ".into());
                instructions.push("<Enter>".yellow().bold());
            }

            if self.contains(Self::RETRY) {
                instructions.push(" Re-try ".into());
                instructions.push("<Enter>".yellow().bold());
            }

            if self.contains(Self::BACK) {
                instructions.push(" Back ".into());
                instructions.push("<Esc>".yellow().bold());
            }

            if self.contains(Self::RESET) {
                instructions.push(" Reset ".into());
                instructions.push("<Esc>".yellow().bold());
            }

            if self.contains(Self::QUIT) {
                instructions.push(" Quit ".into());
                instructions.push("<Alt-Q>".yellow().bold());
            }

            instructions.push(" ".into());

            Line::from(instructions)
        })
    }
}
