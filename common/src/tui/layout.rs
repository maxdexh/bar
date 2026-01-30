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
impl<T> Vec2<T> {
    pub fn combine<U, R>(self, other: Vec2<U>, mut f: impl FnMut(T, U) -> R) -> Vec2<R> {
        Vec2 {
            x: f(self.x, other.x),
            y: f(self.y, other.y),
        }
    }
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
    pub(super) widgets: Vec<(Area, InteractElem)>,
    pub(super) last_mouse_pos: Option<Vec2<u16>>,
    pub(super) last_hover_elem: Option<InteractElem>,
}

pub struct MouseEventResult {
    pub interact: InteractArgs,
    pub callback: Option<InteractCallback>,
    pub empty: bool,
    pub changed: bool,
    pub rerender: bool,
    pub pix_location: Vec2<u32>,
}

impl RenderedLayout {
    pub(super) fn insert(&mut self, area: Area, elem: &InteractElem) {
        self.widgets.push((area, elem.clone()));
    }

    pub fn ext_focus_loss(&mut self) -> bool {
        let changed = self.last_hover_elem.is_some();
        self.last_mouse_pos = None;
        self.last_hover_elem = None;
        changed
    }

    pub fn interpret_mouse_event(
        &mut self,
        event: crossterm::event::MouseEvent,
        font_size: Vec2<u16>,
    ) -> MouseEventResult {
        use crossterm::event::*;

        let MouseEvent {
            kind,
            column,
            row,
            modifiers: _,
        } = event;

        let pos = Vec2 { x: column, y: row };

        self.last_mouse_pos = Some(pos);

        type DR = Direction;
        type IK = InteractKind;
        type MK = crossterm::event::MouseEventKind;

        let kind = match kind {
            MK::Down(button) => IK::Click(button.into()),
            MK::ScrollDown => IK::Scroll(DR::Down),
            MK::ScrollUp => IK::Scroll(DR::Up),
            MK::ScrollLeft => IK::Scroll(DR::Left),
            MK::ScrollRight => IK::Scroll(DR::Right),
            MK::Moved | MK::Up(_) | MK::Drag(_) => IK::Hover,
        };

        let interact = InteractArgs { kind };

        let font_w = u32::from(font_size.x);
        let font_h = u32::from(font_size.y);

        let Some((area, elem)) = self.widgets.iter().find(|(r, _)| r.contains(pos)) else {
            let cur = self.last_hover_elem.take();
            return MouseEventResult {
                interact,
                empty: true,
                callback: None,
                pix_location: Vec2 {
                    x: u32::from(pos.x) * font_w,
                    y: u32::from(pos.y) * font_h,
                },
                changed: cur.is_some(),
                rerender: cur.is_some_and(|it| it.hovered.is_some()),
            };
        };

        let pix_location = {
            Vec2 {
                x: u32::from(area.pos.x) * font_w + u32::from(area.size.x) * font_w / 2,
                y: u32::from(area.pos.y) * font_h + u32::from(area.size.y) * font_h / 2,
            }
        };

        let prev = self.last_hover_elem.replace(elem.clone());

        let changed = !prev
            .as_ref()
            .is_some_and(|it| it.inner.is_identical(&elem.inner));

        let rerender = changed
            && (prev.as_ref().is_some_and(|it| it.hovered.is_some()) || elem.hovered.is_some());

        MouseEventResult {
            interact,
            callback: Some(elem.callback.clone()),
            empty: false,
            changed,
            rerender,
            pix_location,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
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
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum InteractKind {
    Click(MouseButton),
    Scroll(Direction),
    Hover,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
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
