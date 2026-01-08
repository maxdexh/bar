use serde::{Deserialize, Serialize};

use crate::{data::Position32, tui::*};

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
    pub fn get_mut(&mut self, axis: Axis) -> &mut T {
        let Self { x, y } = self;
        match axis {
            Axis::X => x,
            Axis::Y => y,
        }
    }
    pub fn get(mut self, axis: Axis) -> T
    where
        T: Copy,
    {
        *self.get_mut(axis)
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Axis {
    X,
    Y,
}

#[derive(Debug, Default, Clone)]
pub struct RenderedLayout {
    pub widgets: Vec<(Area, InteractTag)>,
}
impl RenderedLayout {
    pub fn insert(&mut self, rect: Area, widget: InteractTag) {
        self.widgets.push((rect, widget));
    }

    // TODO: Delay until hover
    pub fn interpret_mouse_event(
        &mut self,
        event: crossterm::event::MouseEvent,
        font_size: Vec2<u16>,
    ) -> Option<TuiInteract> {
        use crossterm::event::*;

        let MouseEvent {
            kind,
            column,
            row,
            modifiers: _,
        } = event;
        let pos = Vec2 { x: column, y: row };

        let (area, tag) = self
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

        Some(TuiInteract {
            location: {
                let font_w = u32::from(font_size.x);
                let font_h = u32::from(font_size.y);
                Position32 {
                    x: u32::from(area.pos.x) * font_w + u32::from(area.size.x) * font_w / 2,
                    y: u32::from(area.pos.y) * font_h + u32::from(area.size.y) * font_h / 2,
                }
            },
            target: tag.cloned(),
            kind,
        })
    }
}
pub type TuiInteract = crate::data::InteractGeneric<Option<InteractTag>>;

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
    pub fn query() -> anyhow::Result<Self> {
        let crossterm::terminal::WindowSize {
            rows,
            columns,
            width,
            height,
        } = crossterm::terminal::window_size()?;
        if width == 0 || height == 0 {
            anyhow::bail!("Terminal does not support window_size");
        }
        Ok(Self {
            cell_size: Vec2 {
                x: columns,
                y: rows,
            },
            pix_size: Vec2 {
                x: width,
                y: height,
            },
        })
    }
}
