use std::sync::Arc;

use ratatui::{
    Frame, Terminal,
    prelude::*,
    widgets::{self, Paragraph},
};
use ratatui_image::{FontSize, picker::Picker};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::{
    data::{InteractGeneric, InteractKind, Location},
    tui::{self, Element, InteractTag},
    utils::rect_center,
};
pub type PanelInteract = InteractGeneric<Option<InteractTag>>;

// TODO: Partial updates?
#[derive(Serialize, Deserialize)]
pub enum PanelEvent {
    Interact(PanelInteract),
}
#[derive(Serialize, Deserialize)]
pub enum PanelUpdate {
    Display(tui::Tui),
}
// FIXME: Make private
#[derive(Debug, Default)]
pub struct RenderedLayout {
    pub widgets: Vec<(Rect, InteractTag)>,
}
impl RenderedLayout {
    pub fn insert(&mut self, rect: Rect, widget: InteractTag) {
        self.widgets.push((rect, widget));
    }

    // TODO: Delay until hover
    pub fn interpret_mouse_event(
        &mut self,
        event: crossterm::event::MouseEvent,
        font_size: FontSize,
    ) -> Option<PanelInteract> {
        use crossterm::event::*;

        let MouseEvent {
            kind,
            column,
            row,
            modifiers: _,
        } = event;
        let pos = Position { x: column, y: row };

        let (rect, widget) = self
            .widgets
            .iter()
            .find(|(r, _)| r.contains(pos))
            .map_or_else(
                || {
                    (
                        Rect {
                            x: pos.x,
                            y: pos.y,
                            ..Default::default()
                        },
                        None,
                    )
                },
                |(r, w)| (*r, Some(w)),
            );

        type DR = crate::data::Direction;
        type IK = crate::data::InteractKind;
        type MK = crossterm::event::MouseEventKind;
        let kind = match kind {
            MK::Down(button) => IK::Click(button),
            MK::Moved => IK::Hover,
            MK::ScrollDown => IK::Scroll(DR::Down),
            MK::ScrollUp => IK::Scroll(DR::Up),
            MK::ScrollLeft => IK::Scroll(DR::Left),
            MK::ScrollRight => IK::Scroll(DR::Right),
            MK::Up(_) | MK::Drag(_) => {
                return None;
            }
        };

        Some(PanelInteract {
            location: rect_center(rect, font_size),
            target: widget.cloned(),
            kind,
        })
    }
}

fn convert_color(color: &tui::Color) -> ratatui::style::Color {
    use crate::tui::Color as IC;
    use ratatui::style::Color as OC;
    match *color {
        IC::Reset => OC::Reset,
        IC::Black => OC::Black,
        IC::Red => OC::Red,
        IC::Green => OC::Green,
        IC::Yellow => OC::Yellow,
        IC::Blue => OC::Blue,
        IC::Magenta => OC::Magenta,
        IC::Cyan => OC::Cyan,
        IC::Gray => OC::Gray,
        IC::DarkGray => OC::DarkGray,
        IC::LightRed => OC::LightRed,
        IC::LightGreen => OC::LightGreen,
        IC::LightYellow => OC::LightYellow,
        IC::LightBlue => OC::LightBlue,
        IC::LightMagenta => OC::LightMagenta,
        IC::LightCyan => OC::LightCyan,
        IC::White => OC::White,
        IC::Rgb(r, g, b) => OC::Rgb(r, g, b),
        IC::Indexed(i) => OC::Indexed(i),
    }
}
fn convert_style(style: &tui::Style) -> ratatui::style::Style {
    let tui::Style {
        fg,
        bg,
        modifier,
        underline_color,
    } = style;
    ratatui::style::Style {
        fg: fg.as_ref().map(convert_color),
        bg: bg.as_ref().map(convert_color),
        underline_color: underline_color.as_ref().map(convert_color),
        add_modifier: {
            let tui::Modifier {
                bold,
                dim,
                italic,
                underline,
                hidden,
                strike,
            } = *modifier;
            use ratatui::style::Modifier as OM;
            let mut m = OM::default();
            m.set(OM::BOLD, bold);
            m.set(OM::ITALIC, italic);
            m.set(OM::DIM, dim);
            m.set(OM::UNDERLINED, underline);
            m.set(OM::HIDDEN, hidden);
            m.set(OM::CROSSED_OUT, strike);
            m
        },
        sub_modifier: Default::default(),
    }
}

fn part_to_contraint(
    tui::SubPart { constr, elem }: &tui::SubPart,
    picker: &Picker,
    axis: tui::Axis,
    other_axis_size: u16,
) -> Constraint {
    match (*constr, &elem.kind) {
        (tui::Constraint::Length(n), _) => Constraint::Length(n),
        (tui::Constraint::Fill(n), _) => Constraint::Fill(n),
        (
            tui::Constraint::Auto,
            tui::ElementKind::Image(tui::Image {
                data,
                format,
                cached: loaded, // FIXME: Cache (needs mut)
            }),
        ) => {
            let (font_w, font_h) = picker.font_size();
            // FIXME: WHY
            let mut ratio = match image::load_from_memory_with_format(data, *format) {
                Ok(it) => f64::from(it.width()) / f64::from(it.height()),
                Err(_) => 1.0,
            };
            ratio *= f64::from(font_h) / f64::from(font_w);
            let cells = f64::from(other_axis_size)
                * match axis {
                    tui::Axis::Horizontal => ratio,
                    tui::Axis::Vertical => 1.0 / ratio,
                };
            Constraint::Length(cells.ceil() as u16)
        }
        _ => {
            log::error!("Constraint not implemented for element: {constr:#?}\n{elem:#?}");
            Constraint::Length(0)
        }
    }
}

fn render_recursive(
    picker: &Picker,
    frame: &mut Frame,
    elem: &Element,
    layout: &mut RenderedLayout,
    area: Rect,
) {
    if let Some(tag) = &elem.tag {
        layout.insert(area, tag.clone());
    }
    match &elem.kind {
        tui::ElementKind::Subdivide(tui::Subdiv { axis, parts }) => {
            let areas = Layout::default()
                .direction(match axis {
                    tui::Axis::Horizontal => Direction::Horizontal,
                    tui::Axis::Vertical => Direction::Vertical,
                })
                .constraints(parts.iter().map(|part| {
                    part_to_contraint(
                        part,
                        picker,
                        *axis,
                        match axis {
                            tui::Axis::Horizontal => area.height,
                            tui::Axis::Vertical => area.width,
                        },
                    )
                }))
                .split(area);

            let elements = parts.iter().map(|part| &part.elem);

            assert_eq!(areas.len(), elements.len());
            for (area, elem) in areas.iter().zip(elements) {
                render_recursive(picker, frame, elem, layout, *area);
            }
        }
        tui::ElementKind::Image(tui::Image {
            data,
            format,
            cached: loaded, // FIXME: cache (need mut)
        }) => {
            let Ok(img) = image::load_from_memory_with_format(data, *format)
                .map_err(|err| log::error!("Failed to load image: {err}"))
            else {
                return;
            };
            let Ok(img) = picker
                .new_protocol(img, area, ratatui_image::Resize::Fit(None))
                .map_err(|err| log::error!("Failed to create protocol: {err}"))
            else {
                return;
            };
            frame.render_widget(ratatui_image::Image::new(&img), area);
        }
        tui::ElementKind::Block(tui::Block {
            borders,
            border_set,
            inner,
            border_style,
        }) => {
            let block = widgets::Block::new()
                .borders({
                    let tui::Borders {
                        top,
                        bottom,
                        left,
                        right,
                    } = *borders;
                    let mut borders = widgets::Borders::default();
                    borders.set(widgets::Borders::TOP, top);
                    borders.set(widgets::Borders::BOTTOM, bottom);
                    borders.set(widgets::Borders::LEFT, left);
                    borders.set(widgets::Borders::RIGHT, right);
                    borders
                })
                .border_style(convert_style(border_style))
                .border_set({
                    let tui::LineSet {
                        vertical,
                        horizontal,
                        top_right,
                        top_left,
                        bottom_right,
                        bottom_left,
                        ..
                    } = border_set;
                    ratatui::symbols::border::Set {
                        top_left,
                        top_right,
                        bottom_left,
                        bottom_right,
                        vertical_left: vertical,
                        vertical_right: vertical,
                        horizontal_top: horizontal,
                        horizontal_bottom: horizontal,
                    }
                });
            if let Some(inner) = inner {
                render_recursive(picker, frame, inner, layout, block.inner(area));
            }
            frame.render_widget(block, area);
        }
        tui::ElementKind::PlainText(raw) => {
            frame.render_widget(widgets::Paragraph::new(raw as &str), area);
        }
        tui::ElementKind::Empty => {}
    }
}
// FIXME: Make private
pub fn render(tui: &mut tui::Tui, frame: &mut Frame, picker: &Picker) -> RenderedLayout {
    let mut layout = RenderedLayout::default();
    render_recursive(picker, frame, &tui.root, &mut layout, frame.area());
    layout
}

// TODO: Spawn as needed for monitors
pub async fn main(
    ctrl_tx: mpsc::UnboundedSender<PanelEvent>,
    mut ctrl_rx: mpsc::UnboundedReceiver<PanelUpdate>,
    monitor: Arc<str>, // TODO: Use for monitor-dependent elements
) -> anyhow::Result<()> {
    log::info!("Starting display panel");

    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::cursor::Hide,
        crossterm::event::EnableMouseCapture,
    )?;
    crossterm::terminal::enable_raw_mode()?;

    let picker = Picker::from_query_stdio()?;
    let mut ui = RenderedLayout::default();

    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout().lock()))?;

    // HACK: There is a bug that causes double width characters to be
    // small when rendered by ratatui on kitty, seemingly because
    // the spaces around them are not drawn at the beginning
    // (since the unfilled cell is seen as a space?). The workaround
    // is to fill the buffer with some non-space character.
    if let Err(err) = term.draw(|frame| {
        let area @ Rect { height, width, .. } = frame.area();
        frame.render_widget(
            Paragraph::new(
                std::iter::repeat_n(
                    std::iter::repeat_n('\u{2800}', width as _).chain(Some('\n')),
                    height as _,
                )
                .flatten()
                .collect::<String>(),
            ),
            area,
        );
    }) {
        log::error!("Failed to prefill terminal: {err}")
    }

    enum Upd {
        Ctrl(PanelUpdate),
        Term(crossterm::event::Event),
    }
    let ctrl_stream = futures::stream::poll_fn(move |cx| ctrl_rx.poll_recv(cx)).map(Upd::Ctrl);
    let term_stream = crossterm::event::EventStream::new()
        .filter_map(|res| {
            res.map_err(|err| log::error!("Crossterm stream yielded: {err}"))
                .ok()
        })
        .map(Upd::Term);
    let mut events = term_stream.merge(ctrl_stream);

    while let Some(bar_event) = events.next().await {
        match bar_event {
            Upd::Ctrl(PanelUpdate::Display(mut tui)) => {
                if let Err(err) = term.draw(|frame| ui = render(&mut tui, frame, &picker)) {
                    log::error!("Failed to draw: {err}");
                }
            }
            Upd::Term(event) => match event {
                crossterm::event::Event::Paste(_) => continue,
                crossterm::event::Event::FocusGained => continue,
                crossterm::event::Event::Key(_) => continue,
                crossterm::event::Event::FocusLost => {
                    if let Err(err) = ctrl_tx.send(PanelEvent::Interact(PanelInteract {
                        location: Location::ZERO,
                        target: None,
                        kind: InteractKind::Hover,
                    })) {
                        log::warn!("Failed to send interaction: {err}");
                        break;
                    }
                }
                crossterm::event::Event::Mouse(event) => {
                    let Some(interact) = ui.interpret_mouse_event(event, picker.font_size()) else {
                        continue;
                    };

                    if let Err(err) = ctrl_tx.send(PanelEvent::Interact(interact)) {
                        log::warn!("Failed to send interaction: {err}")
                    }

                    continue;
                }
                crossterm::event::Event::Resize(_, _) => (),
            },
        }
    }

    unreachable!()
}
