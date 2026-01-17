mod render;
pub use render::*;
mod layout;
pub use layout::*;

use anyhow::Context;
use std::sync::Arc;

// FIXME: Split mod by elem kind

trait InteractTagBounds: std::any::Any + std::fmt::Debug + Send + Sync {}
impl<T> InteractTagBounds for T where T: std::any::Any + std::fmt::Debug + Send + Sync {}

#[derive(Debug, Clone)]
pub struct InteractTag {
    inner: Arc<dyn InteractTagBounds>,
}
impl InteractTag {
    pub fn downcast_ref<T: std::any::Any>(&self) -> Option<&T> {
        log::trace!(
            "T={:?}, D={:?}",
            std::any::TypeId::of::<T>(),
            self.inner.type_id(),
        );
        // WARN: Arc<dyn InteractTagBounds> implements InteractTagBounds too,
        // so make sure this is actually a deref!
        <dyn std::any::Any>::downcast_ref(&*self.inner as &dyn std::any::Any)
    }
    pub fn new(inner: impl std::any::Any + std::fmt::Debug + Send + Sync) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }
}

#[derive(Default, Debug, Clone)]
pub enum Elem {
    Stack(Stack),
    Text(Text),
    Image(Image),
    Block(Block),
    Tagged(Box<InteractElem>),
    Shared(Arc<Self>),
    #[default]
    Empty,
}
impl From<Stack> for Elem {
    fn from(value: Stack) -> Self {
        Self::Stack(value)
    }
}
impl From<Image> for Elem {
    fn from(value: Image) -> Self {
        Self::Image(value)
    }
}
impl From<Block> for Elem {
    fn from(value: Block) -> Self {
        Self::Block(value)
    }
}
impl From<InteractElem> for Elem {
    fn from(value: InteractElem) -> Self {
        Self::Tagged(Box::new(value))
    }
}
impl From<Text> for Elem {
    fn from(value: Text) -> Self {
        Self::Text(value)
    }
}
impl From<Arc<Elem>> for Elem {
    fn from(value: Arc<Elem>) -> Self {
        Self::Shared(value)
    }
}

#[derive(Debug, Default, Clone)]
pub struct TextLine {
    pub text: String,
    /// The height of the line. Relevant for the text sizing and graphics protocols.
    /// Only used for layout calculations, including determining where the next line is printed in
    /// a [`Text`] element.
    pub height: u16,
}
impl TextLine {
    pub fn plain(text: String) -> Self {
        if text.contains(['\n', '\x1b']) {
            log::warn!("Plain text line {text:?} should not contain <ESC> or newlines.");
        }
        Self { text, height: 1 }
    }
}

// FIXME: Remove in favor of TextLine
#[derive(Debug, Default, Clone)]
pub struct Text {
    // FIXME: Consider removing this in favor of utility functions for styling and size calculations
    pub style: Style,
    pub lines: Vec<TextLine>,
    /// The width of the longest line in cells. Only used for layout calculations.
    /// Each line is printed as-is, regardless of this value.
    pub width: u16,
}
impl Text {
    pub fn plain(text: impl AsRef<str>) -> Self {
        let text = text.as_ref();
        if text.contains('\x1b') {
            log::warn!("Plain text {text:?} should not contain <ESC>.");
        }
        let mut lines = Vec::with_capacity(text.lines().count());
        let mut width = 0u16;
        for line in text.lines() {
            // FIXME: Use unicode-segmentation
            width = std::cmp::max(width, line.chars().count().try_into().unwrap_or(u16::MAX));
            lines.push(TextLine::plain(line.into()));
        }
        Self {
            lines,
            width,
            style: Default::default(),
        }
    }
    pub fn styled(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
    pub fn centered_symbol(sym: impl std::fmt::Display, width: u16) -> Self {
        Self {
            width,
            style: Default::default(),
            lines: [TextLine {
                height: 1,
                // https://sw.kovidgoyal.net/kitty/text-sizing-protocol/
                // - w      set width to 2
                // - h      ceter the text horizontally
                // - n,d    use fractional scale of 1:1. kitty ignores w without this
                text: format!("\x1b]66;w={width}:h=2:n=1:d=1;{sym}\x07"),
            }]
            .into(),
        }
    }
}

// FIXME: InteractTag should uniquely identify a module instance.
// E.g. store sender/on_interact here directly?
#[derive(Debug, Clone)]
pub struct InteractElem {
    pub payload: InteractPayload,
    pub elem: Elem,
}

#[derive(Clone)]
pub struct Image {
    pub img: image::RgbaImage,
}
impl Image {
    pub fn try_load(data: impl AsRef<[u8]>, format: image::ImageFormat) -> anyhow::Result<Image> {
        image::load_from_memory_with_format(data.as_ref(), format)
            .context("Systray icon has invalid png data")
            .map(|img| Image {
                img: img.into_rgba8(),
            })
    }
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut hasher = std::hash::DefaultHasher::new();
        std::hash::Hasher::write(&mut hasher, &self.img);
        let hash = std::hash::Hasher::finish(&hasher);
        f.debug_tuple("Image").field(&hash).finish()
    }
}
#[derive(Debug, Clone)]
pub struct Block {
    pub borders: Borders,
    pub border_style: Style,
    pub border_set: LineSet,
    pub inner: Option<Box<Elem>>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
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

#[derive(Default, Clone, Copy, Debug)]
pub enum Constr {
    Length(u16),
    Fill(u16),
    #[default]
    Auto,
}
#[derive(Debug, Clone)]
pub struct Stack {
    pub axis: Axis,
    pub parts: Vec<StackItem>,
}
impl Stack {
    pub fn horizontal(parts: impl IntoIterator<Item = StackItem>) -> Self {
        Self {
            axis: Axis::X,
            parts: FromIterator::from_iter(parts),
        }
    }
    pub fn vertical(parts: impl IntoIterator<Item = StackItem>) -> Self {
        Self {
            axis: Axis::Y,
            parts: FromIterator::from_iter(parts),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StackItem {
    pub constr: Constr,
    pub elem: Elem,
}
impl StackItem {
    pub fn spacing(len: u16) -> Self {
        Self::length(len, Elem::Empty)
    }
    pub fn auto(elem: impl Into<Elem>) -> Self {
        Self::new(Constr::Auto, elem)
    }
    pub fn length(len: u16, elem: impl Into<Elem>) -> Self {
        Self::new(Constr::Length(len), elem)
    }
    pub fn new(constr: Constr, elem: impl Into<Elem>) -> Self {
        Self {
            constr,
            elem: elem.into(),
        }
    }
}

// FIXME: Remove
// TODO: Tagging system for partial updates?
#[derive(Debug)]
pub struct Tui {
    pub root: Box<Elem>,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub modifier: Modifier,
    pub underline_color: Option<Color>,
}
pub type Color = crossterm::style::Color;
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Modifier {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub hidden: bool,
    pub strike: bool,
}

#[derive(Default, Debug, Clone)]
pub struct LineSet {
    pub vertical: Arc<str>,
    pub horizontal: Arc<str>,
    pub top_right: Arc<str>,
    pub top_left: Arc<str>,
    pub bottom_right: Arc<str>,
    pub bottom_left: Arc<str>,
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
