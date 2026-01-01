use std::sync::Arc;

// TODO: aarc?
// TODO: ElementKind that maps monitor to element

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct InteractTag(Arc<[u8]>);
impl InteractTag {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(bytes.into())
    }
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Element {
    pub tag: Option<InteractTag>,
    pub kind: ElementKind,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub enum ElementKind {
    Subdivide(Subdiv),
    PlainText(Arc<str>),
    Image(Image),
    Block(Block),
    #[default]
    Empty,
}
impl From<Subdiv> for ElementKind {
    fn from(value: Subdiv) -> Self {
        Self::Subdivide(value)
    }
}
impl From<Image> for ElementKind {
    fn from(value: Image) -> Self {
        Self::Image(value)
    }
}
impl From<Block> for ElementKind {
    fn from(value: Block) -> Self {
        Self::Block(value)
    }
}

// FIXME: Write this as an enum instead, encode as png if needed on serialize
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Image {
    pub data: Vec<u8>,
    pub format: image::ImageFormat,
    #[serde(skip)]
    pub cached: Option<image::DynamicImage>,
}
impl Image {
    pub fn load(&mut self) -> anyhow::Result<&image::DynamicImage> {
        if self.cached.is_some() {
            // HACK: Borrow checker limitation
            return Ok(self.cached.as_ref().unwrap());
        }
        let img = image::load_from_memory(&self.data)?;
        Ok(self.cached.insert(img))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Block {
    pub borders: Borders,
    pub border_style: Style,
    pub border_set: LineSet,
    pub inner: Option<Box<Element>>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Borders {
    pub top: bool,
    pub bottom: bool,
    pub left: bool,
    pub right: bool,
}
impl Borders {
    pub fn all() -> Self {
        Self {
            top: true,
            bottom: true,
            left: true,
            right: true,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Axis {
    Horizontal,
    Vertical,
}
#[derive(Default, Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Constraint {
    Length(u16),
    Fill(u16),
    #[default]
    Auto,
    //Percentage(u16),
    //Min(u16),
    //Max(u16),
    //Ratio(u32, u32),
}
// TODO: Builder
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Subdiv {
    pub axis: Axis,
    pub parts: Box<[SubPart]>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubPart {
    pub constr: Constraint,
    pub elem: Element,
}
impl SubPart {
    pub fn spacing(constr: Constraint) -> Self {
        Self {
            constr,
            elem: Element {
                kind: ElementKind::Empty,
                tag: None,
            },
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    w: u16,
    h: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tui {
    pub root: Element,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub modifier: Modifier,
    pub underline_color: Option<Color>,
}
#[derive(Clone, Copy, Default, Debug, Serialize, Deserialize)]
pub enum Color {
    #[default]
    Reset,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
    Rgb(u8, u8, u8),
    Indexed(u8),
}
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Modifier {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub hidden: bool,
    pub strike: bool,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct LineSet {
    pub vertical: Box<str>,
    pub horizontal: Box<str>,
    pub top_right: Box<str>,
    pub top_left: Box<str>,
    pub bottom_right: Box<str>,
    pub bottom_left: Box<str>,
    pub vertical_left: Box<str>,
    pub vertical_right: Box<str>,
    pub horizontal_down: Box<str>,
    pub horizontal_up: Box<str>,
    pub cross: Box<str>,
}

impl LineSet {
    pub fn normal() -> Self {
        Self {
            vertical: "│".into(),
            horizontal: "─".into(),
            top_right: "┐".into(),
            top_left: "┌".into(),
            bottom_right: "┘".into(),
            bottom_left: "└".into(),
            vertical_left: "┤".into(),
            vertical_right: "├".into(),
            horizontal_down: "┬".into(),
            horizontal_up: "┴".into(),
            cross: "┼".into(),
        }
    }

    #[expect(dead_code)]
    pub fn rounded() -> Self {
        Self {
            top_right: "╮".into(),
            top_left: "╭".into(),
            bottom_right: "╯".into(),
            bottom_left: "╰".into(),
            ..Self::normal()
        }
    }

    #[expect(dead_code)]
    pub fn double() -> Self {
        Self {
            vertical: "║".into(),
            horizontal: "═".into(),
            top_right: "╗".into(),
            top_left: "╔".into(),
            bottom_right: "╝".into(),
            bottom_left: "╚".into(),
            vertical_left: "╣".into(),
            vertical_right: "╠".into(),
            horizontal_down: "╦".into(),
            horizontal_up: "╩".into(),
            cross: "╬".into(),
        }
    }

    pub fn thick() -> Self {
        Self {
            vertical: "┃".into(),
            horizontal: "━".into(),
            top_right: "┓".into(),
            top_left: "┏".into(),
            bottom_right: "┛".into(),
            bottom_left: "┗".into(),
            vertical_left: "┫".into(),
            vertical_right: "┣".into(),
            horizontal_down: "┳".into(),
            horizontal_up: "┻".into(),
            cross: "╋".into(),
        }
    }

    #[expect(dead_code)]
    pub fn light_double_dashed() -> Self {
        Self {
            vertical: "╎".into(),
            horizontal: "╌".into(),
            ..Self::normal()
        }
    }

    #[expect(dead_code)]
    pub fn heavy_double_dashed() -> Self {
        Self {
            vertical: "╏".into(),
            horizontal: "╍".into(),
            ..Self::thick()
        }
    }

    #[expect(dead_code)]
    pub fn light_triple_dashed() -> Self {
        Self {
            vertical: "┆".into(),
            horizontal: "┄".into(),
            ..Self::normal()
        }
    }

    #[expect(dead_code)]
    pub fn heavy_triple_dashed() -> Self {
        Self {
            vertical: "┇".into(),
            horizontal: "┅".into(),
            ..Self::thick()
        }
    }

    #[expect(dead_code)]
    pub fn light_quadruple_dashed() -> Self {
        Self {
            vertical: "┊".into(),
            horizontal: "┈".into(),
            ..Self::normal()
        }
    }

    #[expect(dead_code)]
    pub fn heavy_quadruple_dashed() -> Self {
        Self {
            vertical: "┋".into(),
            horizontal: "┉".into(),
            ..Self::thick()
        }
    }
}

#[derive(Clone, Copy)]
pub struct SizingContext {
    font_size: Size,
    div_w: Option<u16>,
    div_h: Option<u16>,
}

impl Tui {
    fn calc_size(&mut self, ctx: SizingContext) -> anyhow::Result<Size> {
        self.root.calc_auto_size(ctx)
    }
}
impl Element {
    pub fn calc_auto_size(&mut self, ctx: SizingContext) -> anyhow::Result<Size> {
        self.kind.calc_auto_size(ctx)
    }
}
impl ElementKind {
    pub fn calc_auto_size(&mut self, ctx: SizingContext) -> anyhow::Result<Size> {
        auto_size_invariants(ctx, || match self {
            Self::Subdivide(subdiv) => subdiv.calc_auto_size(ctx),
            Self::PlainText(text) => {
                let mut size = Size::default();
                for line in text.lines() {
                    size.w = size
                        .w
                        .max(line.chars().count().try_into().unwrap_or(u16::MAX));
                    size.h = size.h.saturating_add(1);
                }
                Ok(size)
            }
            Self::Image(image) => image.calc_auto_size(ctx),
            Self::Block(block) => block.calc_auto_size(ctx),
            Self::Empty => Ok(Size::default()),
        })
    }
}
impl Image {
    fn calc_auto_size(&mut self, ctx: SizingContext) -> anyhow::Result<Size> {
        auto_size_invariants(ctx, || {
            let mut fit = |axis, other_axis_size| {
                let Size {
                    w: font_w,
                    h: font_h,
                } = ctx.font_size;

                let it = self.load()?;
                let mut ratio = f64::from(it.width()) / f64::from(it.height());
                ratio *= f64::from(font_h) / f64::from(font_w);
                let cells = f64::from(other_axis_size)
                    * match axis {
                        Axis::Horizontal => ratio,
                        Axis::Vertical => 1.0 / ratio,
                    };

                Ok::<_, anyhow::Error>(cells.ceil() as u16)
            };
            Ok(match (ctx.div_w, ctx.div_h) {
                (Some(w), Some(h)) => Size { w, h },
                (Some(w), None) => Size {
                    w,
                    h: fit(Axis::Vertical, w)?,
                },
                (None, Some(h)) => Size {
                    h,
                    w: fit(Axis::Horizontal, h)?,
                },
                (None, None) => anyhow::bail!("Cannot fit image without a fixed dimension"),
            })
        })
    }
}
impl Subdiv {
    fn calc_auto_size(&mut self, ctx: SizingContext) -> anyhow::Result<Size> {
        auto_size_invariants(ctx, || {
            let mut size = Size::default();
            for SubPart { constr, elem } in &mut self.parts {
                let elem_size = match constr {
                    Constraint::Length(l) => elem.calc_auto_size(match self.axis {
                        Axis::Horizontal => SizingContext {
                            div_w: Some(*l),
                            ..ctx
                        },
                        Axis::Vertical => SizingContext {
                            div_h: Some(*l),
                            ..ctx
                        },
                    })?,
                    Constraint::Fill(_) => {
                        let mut it = elem.calc_auto_size(match self.axis {
                            Axis::Horizontal => SizingContext { div_w: None, ..ctx },
                            Axis::Vertical => SizingContext { div_h: None, ..ctx },
                        })?;
                        match self.axis {
                            Axis::Horizontal => it.w = 0,
                            Axis::Vertical => it.h = 0,
                        }
                        it
                    }
                    Constraint::Auto => elem.calc_auto_size(ctx)?,
                };
                let horiz = (&mut size.w, elem_size.w);
                let vert = (&mut size.h, elem_size.h);
                let ((adst, asrc), (mdst, msrc)) = match self.axis {
                    Axis::Horizontal => (horiz, vert),
                    Axis::Vertical => (vert, horiz),
                };
                *adst += asrc;
                *mdst = msrc.max(*mdst);
            }
            Ok(size)
        })
    }
}
impl Block {
    fn calc_auto_size(&mut self, ctx: SizingContext) -> anyhow::Result<Size> {
        auto_size_invariants(ctx, || {
            let mut size = self
                .inner
                .as_mut()
                .map(|it| it.calc_auto_size(ctx))
                .transpose()?
                .unwrap_or_default();
            let Borders {
                top,
                bottom,
                left,
                right,
            } = self.borders;
            size.w = size.w.saturating_add(u16::from(left) + u16::from(right));
            size.h = size.h.saturating_add(u16::from(top) + u16::from(bottom));
            Ok(size)
        })
    }
}
fn auto_size_invariants(
    ctx: SizingContext,
    f: impl FnOnce() -> anyhow::Result<Size>,
) -> anyhow::Result<Size> {
    if let (Some(w), Some(h)) = (ctx.div_w, ctx.div_h) {
        return Ok(Size { w, h });
    }
    let mut size = f()?;
    if let Some(w) = ctx.div_w {
        size.w = w;
    }
    if let Some(h) = ctx.div_h {
        size.h = h;
    }
    Ok(size)
}
