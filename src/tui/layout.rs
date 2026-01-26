use serde::{Deserialize, Serialize};

use crate::tui::*;

#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct Area {
    pub pos: Vec2<u16>,
    pub size: Vec2<u16>,
}
impl Area {
    pub fn y_bottom(&self) -> u16 {
        self.pos.y.saturating_add(self.size.y).saturating_sub(1)
    }
    pub fn x_right(&self) -> u16 {
        self.pos.x.saturating_add(self.size.x).saturating_sub(1)
    }
    pub fn contains(self, pos: Vec2<u16>) -> bool {
        pos.x
            .checked_sub(self.pos.x)
            .is_some_and(|it| it < self.size.x)
            && pos
                .y
                .checked_sub(self.pos.y)
                .is_some_and(|it| it < self.size.y)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vec2<T> {
    pub x: T,
    pub y: T,
}
impl<T> std::ops::Index<Axis> for Vec2<T> {
    type Output = T;

    fn index(&self, index: Axis) -> &Self::Output {
        let Self { x, y } = self;
        match index {
            Axis::X => x,
            Axis::Y => y,
        }
    }
}
impl<T> std::ops::IndexMut<Axis> for Vec2<T> {
    fn index_mut(&mut self, index: Axis) -> &mut Self::Output {
        let Self { x, y } = self;
        match index {
            Axis::X => x,
            Axis::Y => y,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Axis {
    X,
    Y,
}
impl Axis {
    pub fn other(self) -> Self {
        match self {
            Self::X => Self::Y,
            Self::Y => Self::X,
        }
    }
}

#[derive(Debug)]
pub struct RenderedLayout {
    pub(super) widgets: Vec<(Area, Elem)>,
    pub(super) last_hover: Option<Vec2<u16>>,
    pub(super) last_hover_id: Option<u64>,
}

#[derive(Debug)]
pub enum MouseEventResult {
    Ignore,
    HoverChanged,
    HoverEmpty,
    Interact {
        pix_location: Vec2<u32>,
        interact: Option<(InteractCallback, InteractArgs)>,
        tooltip: Option<(HoverCallback, HoverArgs)>,
    },
    InteractEmpty,
    HoverTooltip {
        pix_location: Vec2<u32>,
        tooltip: HoverCallback,
        args: HoverArgs,
    },
}

impl RenderedLayout {
    pub fn insert(&mut self, area: Area, elem: &Elem) {
        if elem.hovered.is_some() || elem.tooltip.is_some() || elem.interact.is_some() {
            self.widgets.push((area, elem.clone()))
        }
    }

    pub fn reset_hover(&mut self) -> bool {
        let changed = self.last_hover_id.is_some();
        self.last_hover = None;
        self.last_hover_id = None;
        changed
    }

    // FIXME: Find smallest area that contains location
    pub fn interpret_mouse_event(
        &mut self,
        event: crossterm::event::MouseEvent,
        font_size: Vec2<u16>,
    ) -> MouseEventResult {
        log::debug!("{event:?} {self:?}");
        use crossterm::event::*;

        let MouseEvent {
            kind,
            column,
            row,
            modifiers: _,
        } = event;
        let pos = Vec2 { x: column, y: row };

        let (area, elem) = self
            .widgets
            .iter()
            .find(|(r, _)| r.contains(pos))
            .map_or_else(
                || {
                    (
                        Area {
                            pos,
                            size: Default::default(),
                        },
                        None,
                    )
                },
                |(r, w)| (*r, Some(w)),
            );

        type DR = Direction;
        type IK = InteractKind;
        type MK = crossterm::event::MouseEventKind;

        let pix_location = {
            let font_w = u32::from(font_size.x);
            let font_h = u32::from(font_size.y);
            Vec2 {
                x: u32::from(area.pos.x) * font_w + u32::from(area.size.x) * font_w / 2,
                y: u32::from(area.pos.y) * font_h + u32::from(area.size.y) * font_h / 2,
            }
        };

        if let MK::Moved = kind {
            self.last_hover = Some(pos);
            let Some(Elem {
                hover_id: Some(hover_id),
                tooltip,
                ..
            }) = elem
            else {
                return if self.last_hover_id.take().is_some() {
                    MouseEventResult::HoverEmpty
                } else {
                    MouseEventResult::Ignore
                };
            };
            if self.last_hover_id.replace(*hover_id) == Some(*hover_id) {
                return MouseEventResult::Ignore;
            }
            let Some(tt) = tooltip else {
                return MouseEventResult::HoverChanged;
            };
            MouseEventResult::HoverTooltip {
                pix_location,
                tooltip: tt.clone(),
                args: HoverArgs { _p: () },
            }
        } else {
            let kind = match kind {
                MK::Moved => unreachable!(),
                MK::Down(button) => IK::Click(button.into()),
                MK::ScrollDown => IK::Scroll(DR::Down),
                MK::ScrollUp => IK::Scroll(DR::Up),
                MK::ScrollLeft => IK::Scroll(DR::Left),
                MK::ScrollRight => IK::Scroll(DR::Right),
                MK::Up(_) | MK::Drag(_) => {
                    return MouseEventResult::Ignore;
                }
            };
            let Some(elem) = elem else {
                return MouseEventResult::InteractEmpty;
            };
            MouseEventResult::Interact {
                pix_location,
                interact: elem
                    .interact
                    .as_ref()
                    .map(|cb| (cb.clone(), InteractArgs { kind, _p: () })),
                tooltip: elem
                    .tooltip
                    .as_ref()
                    .map(|tt| (tt.clone(), HoverArgs { _p: () })),
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}
impl From<crossterm::event::MouseButton> for MouseButton {
    fn from(value: crossterm::event::MouseButton) -> Self {
        type MB = crossterm::event::MouseButton;
        match value {
            MB::Left => Self::Left,
            MB::Right => Self::Right,
            MB::Middle => Self::Middle,
        }
    }
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum InteractKind {
    Click(MouseButton),
    Scroll(Direction),
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sizes {
    pub cell_size: Vec2<u16>,
    pub pix_size: Vec2<u16>,
}
impl Sizes {
    pub fn font_size(self) -> Vec2<u16> {
        let Self {
            cell_size: Vec2 { x: w, y: h },
            pix_size: Vec2 { x: pw, y: ph },
        } = self;
        Vec2 {
            x: pw / w,
            y: ph / h,
        }
    }
    pub fn query() -> anyhow::Result<Option<Self>> {
        let crossterm::terminal::WindowSize {
            rows,
            columns,
            width,
            height,
        } = crossterm::terminal::window_size()?;
        if width == 0 || height == 0 {
            return Ok(None);
        }
        Ok(Some(Self {
            cell_size: Vec2 {
                x: columns,
                y: rows,
            },
            pix_size: Vec2 {
                x: width,
                y: height,
            },
        }))
    }
}
