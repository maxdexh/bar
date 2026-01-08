use std::sync::Arc;

// FIXME: Split mod by elem kind

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InteractTag(Arc<[u8]>);
impl InteractTag {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(bytes.into())
    }
}
impl std::fmt::Debug for InteractTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut hasher = std::hash::DefaultHasher::new();
        std::hash::Hasher::write(&mut hasher, &self.0);
        let hash = std::hash::Hasher::finish(&hasher);
        f.debug_tuple("InteractTag").field(&hash).finish()
    }
}

#[derive(Default, Debug)]
pub enum Elem {
    Stack(Stack),
    Text(Text),
    Image(Image),
    Block(Block),
    Tagged(Box<InteractElem>),
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

#[derive(Debug, Default)]
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

#[derive(Debug, Default)]
pub struct Text {
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
}

#[derive(Debug)]
pub struct InteractElem {
    pub tag: InteractTag,
    pub elem: Elem,
}
impl InteractElem {
    pub fn new(tag: InteractTag, elem: impl Into<Elem>) -> Self {
        Self {
            tag,
            elem: elem.into(),
        }
    }
}

pub struct Image {
    pub data: Vec<u8>,
    pub format: image::ImageFormat,
    pub cached: Option<image::RgbaImage>,
}
impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut hasher = std::hash::DefaultHasher::new();
        std::hash::Hasher::write(&mut hasher, &self.data);
        let hash = std::hash::Hasher::finish(&hasher);
        f.debug_tuple("Image")
            .field(&self.format)
            .field(&hash)
            .finish()
    }
}
impl Image {
    pub fn load(&mut self) -> anyhow::Result<&image::RgbaImage> {
        if self.cached.is_some() {
            // HACK: Borrow checker limitation
            return Ok(self.cached.as_ref().unwrap());
        }
        let img = image::load_from_memory_with_format(&self.data, self.format)?;
        Ok(self.cached.insert(img.into_rgba8()))
    }
}
#[derive(Debug)]
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
#[derive(Debug)]
pub struct Stack {
    pub axis: Axis,
    pub parts: Box<[StackItem]>,
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

#[derive(Debug)]
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

#[derive(Default, Debug)]
pub struct LineSet {
    pub vertical: Box<str>,
    pub horizontal: Box<str>,
    pub top_right: Box<str>,
    pub top_left: Box<str>,
    pub bottom_right: Box<str>,
    pub bottom_left: Box<str>,
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
mod render;
pub use render::*;
mod layout;
pub use layout::*;
