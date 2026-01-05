use serde::{Deserialize, Serialize};

use crate::{data::Position32, tui::*};

#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct Area {
    pub pos: Position,
    pub size: Size,
}
impl Area {
    pub fn y_bottom(&self) -> u16 {
        self.pos.y.saturating_add(self.size.h).saturating_sub(1)
    }
    pub fn x_right(&self) -> u16 {
        self.pos.x.saturating_add(self.size.w).saturating_sub(1)
    }
    pub fn contains(self, pos: Position) -> bool {
        pos.x
            .checked_sub(self.pos.x)
            .is_some_and(|it| it < self.size.w)
            && pos
                .y
                .checked_sub(self.pos.y)
                .is_some_and(|it| it < self.size.h)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub x: u16,
    pub y: u16,
}
impl Position {
    pub fn get_mut(&mut self, axis: Axis) -> &mut u16 {
        let Self { x, y } = self;
        match axis {
            Axis::Horizontal => x,
            Axis::Vertical => y,
        }
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub w: u16,
    pub h: u16,
}
impl Size {
    pub fn get_mut(&mut self, axis: Axis) -> &mut u16 {
        let Self { w, h } = self;
        match axis {
            Axis::Horizontal => w,
            Axis::Vertical => h,
        }
    }
    pub fn get(mut self, axis: Axis) -> u16 {
        *self.get_mut(axis)
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Axis {
    Horizontal,
    Vertical,
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
        font_size: Size,
    ) -> Option<TuiInteract> {
        use crossterm::event::*;

        let MouseEvent {
            kind,
            column,
            row,
            modifiers: _,
        } = event;
        let pos = Position { x: column, y: row };

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
                let font_w = u32::from(font_size.w);
                let font_h = u32::from(font_size.h);
                Position32 {
                    x: u32::from(area.pos.x) * font_w + u32::from(area.size.w) * font_w / 2,
                    y: u32::from(area.pos.y) * font_h + u32::from(area.size.h) * font_h / 2,
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
    pub cell_size: Size,
    pub pix_size: Size,
}
impl Sizes {
    pub fn font_size(self) -> Size {
        let Self {
            cell_size: Size { w, h },
            pix_size: Size { w: pw, h: ph },
        } = self;
        Size {
            w: pw / w,
            h: ph / h,
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
            cell_size: Size {
                w: columns,
                h: rows,
            },
            pix_size: Size {
                w: width,
                h: height,
            },
        })
    }
}
