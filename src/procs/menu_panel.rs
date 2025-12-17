use std::sync::Arc;

use anyhow::anyhow;
use futures::Stream;
use ratatui::{
    Terminal,
    layout::{Rect, Size},
    prelude::*,
    widgets::{Block, Paragraph},
};
use ratatui_image::{FontSize, picker::Picker};
use serde::{Deserialize, Serialize};
use system_tray::item::Tooltip;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;

use crate::{
    clients::tray::{TrayMenuExt, TrayMenuInteract},
    data::{ActiveMonitorInfo, InteractKind, Location},
    utils::{IpcReceiver, IpcSender, ResultExt as _, fused_lossy_stream, rect_center},
};

#[derive(Serialize, Deserialize, Debug)]
pub enum MenuEvent {
    Interact(Interact),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MenuWatcherEvents {
    Hide,
}
impl MenuWatcherEvents {
    pub fn listen_from(mut unix: tokio::net::UnixStream) -> impl Stream<Item = Self> {
        use tokio::io::AsyncReadExt;
        let (tx, rx) = broadcast::channel(10);
        tokio::spawn(async move {
            loop {
                let Some(it) = unix
                    .read_u8()
                    .await
                    .ok_or_log("Failed to read from watcher stream")
                else {
                    break;
                };
                let parsed = match it {
                    0 => MenuWatcherEvents::Hide,
                    _ => {
                        log::error!("Unknown event");
                        continue;
                    }
                };

                if tx.send(parsed).is_err() {
                    log::warn!("Watcher channel closed");
                    break;
                }
            }
        });
        fused_lossy_stream(rx)
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum MenuUpdate {
    Watcher(MenuWatcherEvents),
    UnfocusMenu,
    SwitchSubject {
        menu: Menu,
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
        log::trace!("RESIZE SUCCESSFUL");
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

                        if let Some(img) =
                            image::codecs::png::PngDecoder::new(std::io::Cursor::new(icon_data))
                                .and_then(image::DynamicImage::from_decoder)
                                .ok_or_log("Invalid icon data")
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
    ctrl_tx: IpcSender<MenuEvent>,
    ctrl_rx: IpcReceiver<MenuUpdate>,
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

    enum Upd {
        Ctrl(MenuUpdate),
        Term(crossterm::event::Event),
    }

    let term_stream = crossterm::event::EventStream::new()
        .filter_map(|res| res.ok_or_log("Crossterm stream yielded"))
        .map(Upd::Term);
    let mut menu_events = term_stream.merge(ctrl_rx.into_stream().map(Upd::Ctrl));

    while let Some(menu_event) = menu_events.next().await {
        let mut new_geometry = geometry.clone();

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
        match menu_event {
            Upd::Ctrl(update) => {
                match update {
                    MenuUpdate::Watcher(MenuWatcherEvents::Hide) => {
                        switch_subject(&mut cur_menu, Menu::None, Location::ZERO);
                    }
                    MenuUpdate::UnfocusMenu => {
                        if cur_menu.close_on_unfocus() != Some(true) {
                            continue;
                        }
                        if !switch_subject(&mut cur_menu, Menu::None, Location::ZERO) {
                            continue;
                        }
                    }
                    MenuUpdate::ActiveMonitor(ams) => new_geometry.monitor = ams,
                    MenuUpdate::SwitchSubject { menu, location } => {
                        // Do not replace a menu with a tooltip
                        if menu.close_on_unfocus() == Some(true)
                            && menu.close_on_unfocus() == Some(false)
                        {
                            continue;
                        }
                        if !switch_subject(&mut cur_menu, menu, location) {
                            continue;
                        }
                    }
                    MenuUpdate::UpdateTrayMenu(addr, tmenu) => {
                        if let Menu::TrayContext { addr: a, tmenu: t } = &mut cur_menu
                            && *a == addr
                        {
                            *t = tmenu;
                        } else {
                            continue;
                        }
                    }
                    MenuUpdate::UpdateTrayTooltip(addr, tt) => {
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
                    MenuUpdate::RemoveTray(addr) => {
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
                    MenuUpdate::ConnectTrayMenu { addr, menu_path } => {
                        if let Menu::TrayContext { addr: a, tmenu: tm } = &mut cur_menu
                            && a as &str == addr.as_str()
                        {
                            tm.menu_path = menu_path.map(Into::into);
                        } else {
                            continue;
                        }
                    }
                }
            }
            Upd::Term(event) => {
                match event {
                    crossterm::event::Event::Paste(_) => continue,
                    crossterm::event::Event::FocusGained | crossterm::event::Event::Mouse(_)
                        if cur_menu.close_on_unfocus() == Some(true) =>
                    {
                        // If we have a tooltip open and the cursor moves into it, that means
                        // we missed the cursor moving off the icon out of the bar, so we hide
                        // it right away
                        switch_subject(&mut cur_menu, Menu::None, Location::ZERO);
                    }
                    crossterm::event::Event::FocusLost | crossterm::event::Event::FocusGained => {
                        continue;
                    }
                    crossterm::event::Event::Key(_) => continue,
                    crossterm::event::Event::Mouse(event) => {
                        let Some(interact) = ui.interpret_mouse_event(event, picker.font_size())
                        else {
                            continue;
                        };

                        ctrl_tx
                            .send(MenuEvent::Interact(interact))
                            .await
                            .ok_or_log("Failed to send interaction");
                    }
                    crossterm::event::Event::Resize(_, _) => {
                        if pending_resize {
                            log::debug!("Pending resize completed");
                            pending_resize = false;
                        } else {
                            continue;
                        }
                    }
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

        if !pending_resize {
            term.draw(|frame| ui = render_menu(&picker, &cur_menu, frame))
                .ok_or_log("Failed to draw");
        }
    }

    unreachable!()
}
