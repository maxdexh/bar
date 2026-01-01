use std::{ffi::OsString, sync::Arc};

use anyhow::anyhow;
use ratatui::{
    Terminal,
    layout::{Rect, Size},
    prelude::*,
    widgets::{Block, Paragraph},
};
use ratatui_image::{FontSize, picker::Picker};
use serde::{Deserialize, Serialize};
use system_tray::item::Tooltip;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::{
    clients::tray::{TrayMenuExt, TrayMenuInteract},
    data::{ActiveMonitorInfo, InteractKind, Location},
    tui,
    utils::rect_center,
};

pub async fn controller_spawn_panel(
    dir: &std::path::Path,
    display: &str,
    envs: Vec<(OsString, OsString)>,
    ev_tx: &tokio::sync::mpsc::UnboundedSender<MenuEvent>,
) -> anyhow::Result<tokio::process::Child> {
    let watcher_py = dir.join("menu_watcher.py");
    tokio::fs::write(&watcher_py, include_bytes!("menu_panel/menu_watcher.py")).await?;

    let watcher_sock_path = dir.join("menu_watcher.sock");

    // FIXME: Consider opening the socket in the watcher instead?
    // Then we could move the socket into the menu.
    let watcher_listener = tokio::net::UnixListener::bind(&watcher_sock_path)?;

    let child = tokio::process::Command::new("kitten")
        .stdout(std::io::stderr())
        .envs(envs)
        .env("BAR_MENU_WATCHER_SOCK", watcher_sock_path)
        .arg("panel")
        .arg({
            let mut arg = OsString::from("-o=watcher=");
            arg.push(watcher_py);
            arg
        })
        .args([
            &format!("--output-name={display}"),
            // Configure remote control via socket
            "-o=allow_remote_control=socket-only",
            "--listen-on=unix:/tmp/kitty-bar-menu-panel.sock",
            // Allow logging to $KITTY_STDIO_FORWARDED
            "-o=forward_stdio=yes",
            // Do not use the system's kitty.conf
            "--config=NONE",
            // Basic look of the menu
            "-o=background_opacity=0.85",
            "-o=background=black",
            "-o=foreground=white",
            // location of the menu
            "--edge=top",
            // disable hiding the mouse
            "-o=mouse_hide_wait=0",
            // Window behavior of the menu panel. Makes panel
            // act as an overlay on top of other windows.
            // We do not want tilers to dedicate space to it.
            // Taken from the args that quick-access-terminal uses.
            "--exclusive-zone=0",
            "--override-exclusive-zone",
            "--layer=overlay",
            // Focus behavior of the panel. Since we cannot tell from
            // mouse events alone when the cursor leaves the panel
            // (since terminal mouse capture only gives us mouse
            // events inside the panel), we need external support for
            // hiding it automatically. We use a watcher to be able
            // to reset the menu state when this happens.
            "--focus-policy=on-demand",
            "--hide-on-focus-loss",
            // Since we control resizes from the program and not from
            // a somewhat continuous drag-resize, debouncing between
            // resize and reloads is completely inappropriate and
            // just results in a larger delay between resize and
            // the old menu content being replaced with the new one.
            "-o=resize_debounce_time=0 0",
            // TODO: Mess with repaint_delay, input_delay
        ])
        .arg(&std::env::current_exe()?)
        .args(["internal", super::INTERNAL_MENU_ARG])
        .kill_on_drop(true)
        .spawn()?;

    let (mut watcher_stream, _) = watcher_listener.accept().await?;
    let ev_tx = ev_tx.clone();
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        loop {
            let Ok(it) = watcher_stream
                .read_u8()
                .await
                .map_err(|err| log::error!("Failed to read from watcher stream: {err}"))
            else {
                break;
            };
            let parsed = match it {
                0 => MenuWatcherEvent::Hide,
                1 => MenuWatcherEvent::Resize,
                _ => {
                    log::error!("Unknown event");
                    continue;
                }
            };

            if let Err(err) = ev_tx.send(MenuEvent::Watcher(parsed)) {
                log::warn!("Failed to send watcher event: {err}");
                break;
            }
        }
    });

    Ok(child)
}

#[derive(Serialize, Deserialize, Debug)]
pub enum MenuEvent {
    Interact(Interact),
    Watcher(MenuWatcherEvent),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MenuWatcherEvent {
    Hide,
    Resize,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum MenuUpdate {
    Watcher(MenuWatcherEvent),
    UnfocusMenu,
    SwitchSubject {
        new_menu: Menu,
        location: Location,
    }, // TODO: Monitor info
    UpdateTrayMenu(Arc<str>, TrayMenuExt),
    UpdateTrayTooltip(Arc<str>, Option<Tooltip>),
    RemoveTray(Arc<str>),
    ConnectTrayMenu {
        addr: String,
        menu_path: Option<String>,
    },
    ActiveMonitor(Option<ActiveMonitorInfo>),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MenuInteractTarget {
    TrayMenu(crate::clients::tray::TrayMenuInteract),
}
impl MenuInteractTarget {
    fn serialize_tag(&self) -> tui::InteractTag {
        tui::InteractTag::from_bytes(&postcard::to_stdvec(self).unwrap())
    }
    fn deserialize_tag(tag: &tui::InteractTag) -> Self {
        postcard::from_bytes(tag.as_bytes()).unwrap()
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Menu {
    TrayContext { addr: Arc<str>, tmenu: TrayMenuExt },
    TrayTooltip { addr: Arc<str>, tooltip: Tooltip },
    None,
}
impl Menu {
    fn is_visible(&self) -> bool {
        !matches!(self, Self::None)
    }
    fn close_on_unfocus(&self) -> Option<bool> {
        Some(match self {
            Self::TrayTooltip { .. } => true,
            Self::TrayContext { .. } => false,
            Self::None => return None,
        })
    }
}

#[derive(Debug, Default)]
pub struct RenderedLayout {
    widgets: Vec<(Rect, MenuInteractTarget)>,
}
type Interact = crate::data::InteractGeneric<MenuInteractTarget>;

impl RenderedLayout {
    pub fn insert(&mut self, rect: Rect, widget: MenuInteractTarget) {
        self.widgets.push((rect, widget));
    }

    pub fn interpret_mouse_event(
        &mut self,
        event: crossterm::event::MouseEvent,
        font_size: FontSize,
    ) -> Option<Interact> {
        use crossterm::event::MouseEventKind as MEK;
        use crossterm::event::*;

        let MouseEvent {
            kind,
            column,
            row,
            modifiers: _,
        } = event;
        let pos = Position { x: column, y: row };

        let (rect, widget) = {
            let mut targets = self.widgets.iter().filter(|(r, _)| r.contains(pos));
            if let Some(found @ (r, w)) = targets.next() {
                if let Some(extra) = targets.next() {
                    log::error!("Multiple widgets contain {pos}: {extra:#?}, {found:#?}");
                }
                (*r, w)
            } else {
                return None;
            }
        };

        let kind = match kind {
            // TODO: Consider using Up instead
            MEK::Down(button) => InteractKind::Click(button),
            MEK::Moved => InteractKind::Hover,
            MEK::Up(_)
            | MEK::ScrollDown
            | MEK::ScrollUp
            | MEK::ScrollLeft
            | MEK::ScrollRight
            | MEK::Drag(_) => {
                return None;
            }
        };

        Some(Interact {
            location: rect_center(rect, font_size),
            target: widget.clone(),
            kind,
        })
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct Geometry {
    size: Size,
    location: Location,
    font_size: FontSize,
    monitor: Option<ActiveMonitorInfo>,
}
async fn adjust_terminal(
    new_geo: &Geometry,
    old_geo: &Geometry,
    socket: &str,
    menu_is_visible: bool,
) -> anyhow::Result<bool> {
    let &Geometry {
        size: Size { width, height },
        location: Location { x, y: _ },
        font_size: (font_w, _),
        ref monitor,
    } = new_geo;

    let mut pending_resize = false;

    async fn run_command(cmd: &mut tokio::process::Command) -> anyhow::Result<()> {
        let output = cmd.stderr(std::process::Stdio::piped()).output().await?;
        if !output.status.success() {
            Err(anyhow!(
                "{cmd:#?} Exited with status {:#?}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ))
        } else {
            Ok(())
        }
    }

    if menu_is_visible
        && new_geo != old_geo
        && let Some(monitor) = monitor
    {
        pending_resize = true;

        log::trace!("Resizing terminal: {new_geo:#?}");

        // NOTE: There is no absolute positioning system, nor a way to directly specify the
        // geometry (since this is controlled by the compositor). So we have to get creative by
        // using the right and left margin to control both position and size of the panel.

        // cap position at monitor's size
        let x = std::cmp::min(x, monitor.width);

        // Find the distance between window edge and center
        // HACK: The margin calculation is always slightly too small, so add a few cells to the
        // calculation
        // TODO: Find out if this needs to increase with the width or if the error is constant
        let half_pix_w = (u32::from(width + 3) * u32::from(font_w)).div_ceil(2);

        // The left margin should be such that half the space is between
        // left margin and x. Use saturating_sub so that the left
        // margin becomes zero if the width would reach outside the screen.
        let mleft = x.saturating_sub(half_pix_w);

        // Get the overshoot, i.e. the amount lost to saturating_sub (we have to account for it
        // in the right margin). For this observe that a.saturating_sub(b) = a - min(a, b) and
        // therefore the overshoot is:
        // a.saturating_sub(b) - (a - b)
        // = a - min(a, b) - (a - b)
        // = b - min(a, b)
        // = b.saturating_sub(a)
        let overshoot = half_pix_w.saturating_sub(x);

        let mright = (monitor.width - x)
            .saturating_sub(half_pix_w)
            .saturating_sub(overshoot);

        // The font size (on which cell->pixel conversion is based) and the monitor's
        // size are in physical pixels. This makes sense because different monitors can
        // have different scales, and the application should not be affected by that
        // (this is not x11 after all).
        // However, panels are bound to a monitor and the margins are in scaled pixels,
        // so we have to make this correction.
        let margin_left = (f64::from(mleft) / monitor.scale) as u32;
        let margin_right = (f64::from(mright) / monitor.scale) as u32;

        // TODO: Timeout
        run_command(
            tokio::process::Command::new("kitten")
                .args([
                    "@",
                    &format!("--to={socket}"),
                    "resize-os-window",
                    "--incremental",
                    "--action=os-panel",
                ])
                .args({
                    let args = [
                        format!("margin-left={margin_left}"),
                        format!("margin-right={margin_right}"),
                        format!("lines={height}"),
                    ];
                    log::info!("Resizing menu: {args:?}");
                    args
                }),
        )
        .await?;
    }

    // TODO: Move to correct monitor
    let action = if menu_is_visible && monitor.is_some() {
        "show"
    } else {
        "hide"
    };
    run_command(tokio::process::Command::new("kitten").args([
        "@",
        &format!("--to={socket}"),
        "resize-os-window",
        &format!("--action={action}"),
    ]))
    .await?;

    Ok(pending_resize)
}

fn text_size(text: &str) -> Size {
    let (width, height) = text.lines().fold((0, 0), |(w, h), line| {
        (w.max(line.chars().count() as u16), h + 1)
    });
    Size { width, height }
}
fn extend_size_down(dst: &mut Size, src: Size) {
    dst.width = dst.width.max(src.width);
    dst.height += src.height;
}

fn render_tray_menu_item(
    picker: &Picker,
    depth: u16,
    item: &system_tray::menu::MenuItem,
    addr: &Arc<str>,
    menu_path: Option<&Arc<str>>,
    out: &mut RenderReq,
) {
    use system_tray::menu::*;
    match item {
        MenuItem { visible: false, .. } => (),
        MenuItem {
            visible: true,
            menu_type: MenuType::Separator,
            ..
        } => match out {
            RenderReq::Precalc(size) => size.height += 1,
            RenderReq::Render(frame, _, area) => {
                let separator_area;
                [separator_area, *area] =
                    Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(*area);
                frame.render_widget(
                    Block::new()
                        .borders(ratatui::widgets::Borders::TOP)
                        .border_style(Color::DarkGray),
                    separator_area,
                );
            }
        },
        MenuItem {
            id,
            menu_type: MenuType::Standard,
            label: Some(label),
            enabled: _,
            visible: true,
            icon_name: _,
            icon_data,
            shortcut: _,
            toggle_type: _,  // TODO
            toggle_state: _, // TODO
            children_display: _,
            disposition: _, // TODO: ???
            submenu,
        } => {
            let square_icon_len = {
                let (font_w, font_h) = picker.font_size();
                font_h.div_ceil(font_w)
            };

            let mut label_size = text_size(label);
            label_size.width += depth;

            // FIXME: Probably better to just shove the image in the first line at
            // normal size rather than this.
            let img_width = if icon_data.is_some() {
                square_icon_len * label_size.height
            } else {
                0
            };

            label_size.width += img_width + 1;

            match out {
                RenderReq::Render(frame, rendered_layout, area) => {
                    let mut text_area;
                    [text_area, *area] = Layout::vertical([
                        Constraint::Length(label_size.height),
                        Constraint::Fill(1),
                    ])
                    .areas(*area);
                    if let Some(icon_data) = icon_data {
                        let [icon_area, _, rest] = Layout::horizontal([
                            Constraint::Length(img_width),
                            Constraint::Length(1),
                            Constraint::Fill(1),
                        ])
                        .areas(text_area);

                        if let Ok(img) =
                            image::codecs::png::PngDecoder::new(std::io::Cursor::new(icon_data))
                                .and_then(image::DynamicImage::from_decoder)
                                .map_err(|err| log::error!("Invalid icon data: {err}"))
                        {
                            frame.render_stateful_widget(
                                ratatui_image::StatefulImage::default(),
                                icon_area,
                                &mut picker.new_resize_protocol(img),
                            );
                        }

                        text_area = rest;
                    }
                    frame.render_widget(
                        Paragraph::new(format!("{}{label}", " ".repeat(depth as _))),
                        text_area,
                    );
                    if let Some(mp) = menu_path {
                        rendered_layout.insert(
                            text_area,
                            MenuInteractTarget::TrayMenu(TrayMenuInteract {
                                addr: addr.clone(),
                                menu_path: mp.clone(),
                                id: *id,
                            }),
                        );
                    }
                }
                RenderReq::Precalc(size) => {
                    extend_size_down(size, label_size);
                }
            }

            if !submenu.is_empty() {
                render_tray_menu(picker, depth + 1, submenu, addr, menu_path, out);
            }
        }

        _ => log::warn!("Unhandled menu item: {item:#?}"),
    }
}
fn tray_menu_item_to_tui(
    depth: u16,
    item: &system_tray::menu::MenuItem,
    addr: &Arc<str>,
    menu_path: Option<&Arc<str>>,
) -> Option<tui::Element> {
    use system_tray::menu::*;
    let main_elem = match item {
        MenuItem { visible: false, .. } => return None,
        MenuItem {
            visible: true,
            menu_type: MenuType::Separator,
            ..
        } => tui::Element {
            tag: None,
            kind: tui::ElementKind::Block(tui::Block {
                borders: tui::Borders {
                    top: true,
                    ..Default::default()
                },
                border_style: tui::Style {
                    fg: Some(tui::Color::DarkGray),
                    ..Default::default()
                },
                border_set: tui::LineSet::normal(),
                inner: None,
            }),
        },

        MenuItem {
            id,
            menu_type: MenuType::Standard,
            label: Some(label),
            enabled: _,
            visible: true,
            icon_name: _,
            icon_data,
            shortcut: _,
            toggle_type: _,  // TODO
            toggle_state: _, // TODO
            children_display: _,
            disposition: _, // TODO: ???
            submenu,
        } => tui::Element {
            tag: menu_path.map(|it| {
                MenuInteractTarget::TrayMenu(TrayMenuInteract {
                    addr: addr.clone(),
                    menu_path: it.clone(),
                    id: *id,
                })
                .serialize_tag()
            }),
            kind: if let Some(icon) = icon_data {
                tui::ElementKind::Subdivide(tui::Subdiv {
                    axis: tui::Axis::Horizontal,
                    // FIXME: Put in the first line instead
                    parts: Box::new([
                        tui::SubPart {
                            constr: tui::Constraint::Auto,
                            elem: tui::Element {
                                tag: None,
                                kind: tui::ElementKind::Image(tui::Image {
                                    data: icon.clone(),
                                    format: image::ImageFormat::Png,
                                    cached: None,
                                }),
                            },
                        },
                        tui::SubPart {
                            constr: tui::Constraint::Auto,
                            elem: tui::Element {
                                tag: None,
                                kind: tui::ElementKind::PlainText(label.as_str().into()),
                            },
                        },
                    ]),
                })
            } else {
                tui::ElementKind::PlainText(label.as_str().into())
            },
        },

        _ => {
            log::error!("Unhandled menu item: {item:#?}");
            return None;
        }
    };

    Some(if item.submenu.is_empty() {
        main_elem
    } else {
        tui::Element {
            tag: None,
            kind: tui::Subdiv {
                axis: tui::Axis::Vertical,
                parts: Box::new([
                    tui::SubPart {
                        constr: tui::Constraint::Auto,
                        elem: main_elem,
                    },
                    tui::SubPart {
                        constr: tui::Constraint::Auto,
                        elem: tray_menu_to_tui(depth + 1, &item.submenu, addr, menu_path),
                    },
                ]),
            }
            .into(),
        }
    })
}

fn render_tray_menu(
    picker: &Picker,
    depth: u16,
    items: &[system_tray::menu::MenuItem],
    addr: &Arc<str>,
    menu_path: Option<&Arc<str>>,
    out: &mut RenderReq,
) {
    if depth > 5 {
        log::error!("Tray menu is nested too deeply, skipping {items:#?}");
        return;
    }

    for item in items {
        render_tray_menu_item(picker, depth, item, addr, menu_path, out);
    }
}
fn tray_menu_to_tui(
    depth: u16,
    items: &[system_tray::menu::MenuItem],
    addr: &Arc<str>,
    menu_path: Option<&Arc<str>>,
) -> tui::Element {
    let subdiv = tui::Subdiv {
        axis: tui::Axis::Vertical,
        parts: items
            .iter()
            .filter_map(|item| {
                Some(tui::SubPart {
                    constr: tui::Constraint::Auto,
                    elem: tray_menu_item_to_tui(depth, item, addr, menu_path)?,
                })
            })
            .collect(),
    };

    tui::Element {
        tag: None,
        kind: tui::ElementKind::Subdivide(tui::Subdiv {
            axis: tui::Axis::Horizontal,
            parts: Box::new([
                tui::SubPart {
                    constr: tui::Constraint::Length(1),
                    elem: tui::Element {
                        kind: tui::ElementKind::Empty,
                        tag: None,
                    },
                },
                tui::SubPart {
                    constr: tui::Constraint::Auto,
                    elem: tui::Element {
                        tag: None,
                        kind: tui::ElementKind::Subdivide(subdiv),
                    },
                },
            ]),
        }),
    }
}

fn render_or_calc(picker: &Picker, menu: &Menu, out: &mut RenderReq) {
    match menu {
        Menu::None => (),
        Menu::TrayContext {
            addr,
            tmenu:
                TrayMenuExt {
                    id: _,
                    menu_path,
                    submenus,
                },
        } => {
            // Draw a frame around the context menu
            match out {
                RenderReq::Render(frame, _, area) => {
                    let block = ratatui::widgets::Block::bordered()
                        .border_style(Color::DarkGray)
                        .border_type(ratatui::widgets::BorderType::Thick);
                    let inner_area = block.inner(*area);
                    frame.render_widget(block, *area);
                    *area = inner_area;
                }
                RenderReq::Precalc(size) => {
                    size.width += 2;
                    size.height += 2;
                }
            }
            // Then render the items
            render_tray_menu(picker, 0, submenus, addr, menu_path.as_ref(), out)
        }
        Menu::TrayTooltip {
            addr: _,
            tooltip:
                Tooltip {
                    icon_name: _,
                    icon_data: _,
                    title,
                    description,
                },
        } => {
            let title_size = text_size(title);
            let desc_size = text_size(description);

            match out {
                RenderReq::Render(frame, _, area) => {
                    let [title_area, desc_area] = Layout::vertical([
                        Constraint::Length(title_size.height),
                        Constraint::Length(desc_size.height),
                    ])
                    .areas(*area);
                    frame.render_widget(
                        Paragraph::new(title.as_str()).centered().bold(),
                        title_area,
                    );
                    frame.render_widget(Paragraph::new(description.as_str()), desc_area);
                }
                RenderReq::Precalc(size) => {
                    extend_size_down(size, title_size);
                    extend_size_down(size, desc_size);
                }
            }
        }
    }
}
pub fn to_tui(menu: &Menu) -> Option<tui::Tui> {
    Some(match menu {
        Menu::None => return None,
        Menu::TrayContext {
            addr,
            tmenu:
                TrayMenuExt {
                    id: _,
                    menu_path,
                    submenus,
                },
        } => {
            // Then render the items
            tui::Tui {
                root: tui::Element {
                    tag: None,
                    kind: tui::ElementKind::Block(tui::Block {
                        borders: tui::Borders::all(),
                        border_style: tui::Style {
                            fg: Some(tui::Color::DarkGray),
                            ..Default::default()
                        },
                        border_set: tui::LineSet::thick(),
                        inner: Some(Box::new(tray_menu_to_tui(
                            0,
                            submenus,
                            addr,
                            menu_path.as_ref(),
                        ))),
                    }),
                },
            }
        }
        Menu::TrayTooltip {
            addr: _,
            tooltip:
                Tooltip {
                    icon_name: _,
                    icon_data: _,
                    title,
                    description,
                },
        } => tui::Tui {
            root: tui::Element {
                tag: None,
                kind: tui::Subdiv {
                    axis: tui::Axis::Vertical,
                    parts: Box::new([
                        tui::SubPart {
                            constr: tui::Constraint::Auto,
                            elem: tui::Element {
                                tag: None,
                                kind: tui::ElementKind::PlainText(title.as_str().into()),
                            },
                        },
                        tui::SubPart {
                            constr: tui::Constraint::Auto,
                            elem: tui::Element {
                                tag: None,
                                kind: tui::ElementKind::PlainText(description.as_str().into()),
                            },
                        },
                    ]),
                }
                .into(),
            },
        },
    })
}
fn calc_size(picker: &Picker, menu: &Menu) -> Size {
    let mut size = Size::default();
    render_or_calc(picker, menu, &mut RenderReq::Precalc(&mut size));
    size
}
fn render_menu(picker: &Picker, menu: &Menu, frame: &mut ratatui::Frame) -> RenderedLayout {
    let mut out = RenderedLayout::default();
    render_or_calc(
        picker,
        menu,
        &mut RenderReq::Render(frame, &mut out, frame.area()),
    );
    out
}

// TODO: Try https://sw.kovidgoyal.net/kitty/launch/#watchers for listening to hide
// and focus loss events.
enum RenderReq<'a, 'b> {
    Render(&'a mut ratatui::Frame<'b>, &'a mut RenderedLayout, Rect),
    Precalc(&'a mut Size),
}

// TODO: Handle hovers by highlighting option
pub async fn main(
    ctrl_tx: mpsc::UnboundedSender<MenuEvent>,
    mut ctrl_rx: mpsc::UnboundedReceiver<MenuUpdate>,
) -> anyhow::Result<()> {
    log::debug!("Starting menu");

    let socket = std::env::var("KITTY_LISTEN_ON")?;
    // NOTE: Avoid --start-as-hidden due to https://github.com/kovidgoyal/kitty/issues/9306
    tokio::process::Command::new("kitten")
        .args(["@", "--to", &socket, "resize-os-window", "--action=hide"])
        .status()
        .await?;

    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::cursor::Hide,
        crossterm::event::EnableMouseCapture,
    )?;
    crossterm::terminal::enable_raw_mode()?;

    let mut cur_menu = Menu::None;
    let picker = Picker::from_query_stdio()?;

    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout().lock()))?;
    let mut ui = RenderedLayout::default();

    let mut geometry = Geometry {
        size: Default::default(),
        location: Location::ZERO,
        font_size: picker.font_size(),
        monitor: None,
    };
    let mut pending_resize = false;

    #[derive(Debug)]
    enum Upd {
        Ctrl(MenuUpdate),
        Term(crossterm::event::Event),
    }

    let term_stream = crossterm::event::EventStream::new()
        .filter_map(|res| {
            res.map_err(|err| log::error!("Crossterm stream yielded: {err}"))
                .ok()
        })
        .map(Upd::Term);
    let ctrl_stream = futures::stream::poll_fn(move |cx| ctrl_rx.poll_recv(cx)).map(Upd::Ctrl);
    let mut menu_events = term_stream.merge(ctrl_stream);

    while let Some(menu_event) = menu_events.next().await {
        let mut new_geometry = geometry.clone();

        // FIXME: Debounce to avoid racy behavior with resizing
        let mut switch_subject = |cur_menu: &mut Menu, new_menu: Menu, location| {
            let is_visible = new_menu.is_visible();
            match (&*cur_menu, &new_menu) {
                (
                    Menu::TrayTooltip { addr, tooltip: _ },
                    Menu::TrayTooltip {
                        addr: addr2,
                        tooltip: _,
                    },
                ) if addr == addr2 => false,
                (Menu::None, Menu::None) => false,
                _ => {
                    *cur_menu = new_menu;
                    if is_visible {
                        new_geometry.location = location;
                    }
                    true
                }
            }
        };

        type TE = crossterm::event::Event;
        match menu_event {
            Upd::Ctrl(MenuUpdate::Watcher(MenuWatcherEvent::Resize))
            | Upd::Term(TE::Resize(_, _)) => {
                if pending_resize {
                    log::debug!("Pending resize completed: {menu_event:?}");
                    pending_resize = false;
                } else {
                    continue;
                }
            }
            Upd::Term(TE::Paste(_)) => continue,
            Upd::Term(TE::FocusGained | crossterm::event::Event::Mouse(_))
                if cur_menu.close_on_unfocus() == Some(true) =>
            {
                // If we have a tooltip open and the cursor moves into it, that means
                // we missed the cursor moving off the icon out of the bar, so we hide
                // it right away
                switch_subject(&mut cur_menu, Menu::None, Location::ZERO);
            }
            Upd::Term(TE::FocusLost | crossterm::event::Event::FocusGained) => {
                continue;
            }
            Upd::Term(TE::Key(_)) => continue,
            Upd::Term(TE::Mouse(event)) => {
                let Some(interact) = ui.interpret_mouse_event(event, picker.font_size()) else {
                    continue;
                };

                if let Err(err) = ctrl_tx.send(MenuEvent::Interact(interact)) {
                    log::warn!("Failed to send interaction: {err}");
                    break;
                }
            }
            Upd::Ctrl(MenuUpdate::Watcher(MenuWatcherEvent::Hide)) => {
                switch_subject(&mut cur_menu, Menu::None, Location::ZERO);
            }
            Upd::Ctrl(MenuUpdate::UnfocusMenu) => {
                if cur_menu.close_on_unfocus() != Some(true) {
                    continue;
                }
                if !switch_subject(&mut cur_menu, Menu::None, Location::ZERO) {
                    continue;
                }
            }
            Upd::Ctrl(MenuUpdate::ActiveMonitor(ams)) => new_geometry.monitor = ams,
            Upd::Ctrl(MenuUpdate::SwitchSubject { new_menu, location }) => {
                // Do not replace a menu with a tooltip
                if new_menu.close_on_unfocus() == Some(true)
                    && cur_menu.close_on_unfocus() == Some(false)
                {
                    continue;
                }
                if !switch_subject(&mut cur_menu, new_menu, location) {
                    continue;
                }
            }
            Upd::Ctrl(MenuUpdate::UpdateTrayMenu(addr, tmenu)) => {
                if let Menu::TrayContext { addr: a, tmenu: t } = &mut cur_menu
                    && *a == addr
                {
                    *t = tmenu;
                } else {
                    continue;
                }
            }
            Upd::Ctrl(MenuUpdate::UpdateTrayTooltip(addr, tt)) => {
                if let Menu::TrayTooltip {
                    addr: a,
                    tooltip: t,
                } = &mut cur_menu
                    && *a == addr
                {
                    match tt {
                        Some(tt) => *t = tt,
                        None => cur_menu = Menu::None,
                    }
                } else {
                    continue;
                }
            }
            Upd::Ctrl(MenuUpdate::RemoveTray(addr)) => {
                if let Menu::TrayContext { addr: a, tmenu: _ }
                | Menu::TrayTooltip {
                    addr: a,
                    tooltip: _,
                } = &cur_menu
                    && *a == addr
                {
                    cur_menu = Menu::None;
                } else {
                    continue;
                }
            }
            Upd::Ctrl(MenuUpdate::ConnectTrayMenu { addr, menu_path }) => {
                if let Menu::TrayContext { addr: a, tmenu: tm } = &mut cur_menu
                    && a as &str == addr.as_str()
                {
                    tm.menu_path = menu_path.map(Into::into);
                } else {
                    continue;
                }
            }
        }

        if cur_menu.is_visible() {
            new_geometry.size = calc_size(&picker, &cur_menu);
        }

        if adjust_terminal(&new_geometry, &geometry, &socket, cur_menu.is_visible()).await? {
            pending_resize = true;
        }
        geometry = new_geometry;

        if !pending_resize
            && let Err(err) = term.draw(|frame| ui = render_menu(&picker, &cur_menu, frame))
        {
            log::error!("Failed to draw: {err}")
        }
    }

    unreachable!()
}
