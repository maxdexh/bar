mod render;
pub use render::*;
mod layout;
pub use layout::*;

use std::{fmt, sync::Arc};

#[derive(Default, Debug, Clone)]
enum ElemKind {
    Print(RawPrint),
    Image(Image),
    Stack(Stack),
    Block(Box<Block>),
    Shared(Arc<Elem>),
    #[default]
    Empty,
}

#[derive(Default, Debug, Clone)]
pub struct Elem {
    kind: ElemKind,
    on_interact: Option<InteractCallback>,
}
impl Elem {
    pub fn empty() -> Self {
        Self {
            kind: ElemKind::Empty,
            ..Default::default()
        }
    }

    pub fn on_interact(mut self, callback: InteractCallback) -> Self {
        self.on_interact = Some(callback);
        self
    }
}
impl From<Stack> for Elem {
    fn from(value: Stack) -> Self {
        Self {
            kind: ElemKind::Stack(value),
            ..Default::default()
        }
    }
}
impl From<Image> for Elem {
    fn from(value: Image) -> Self {
        Self {
            kind: ElemKind::Image(value),
            ..Default::default()
        }
    }
}
impl From<Block> for Elem {
    fn from(value: Block) -> Self {
        Self {
            kind: ElemKind::Block(Box::new(value)),
            ..Default::default()
        }
    }
}
impl From<Arc<Elem>> for Elem {
    fn from(value: Arc<Elem>) -> Self {
        Self {
            kind: ElemKind::Shared(value),
            ..Default::default()
        }
    }
}
impl<D: fmt::Display> From<RawPrint<D>> for Elem {
    fn from(value: RawPrint<D>) -> Self {
        Self {
            kind: ElemKind::Print(value.map_display(|it| it.to_string())),
            ..Default::default()
        }
    }
}
impl From<PlainLines<'_>> for Elem {
    fn from(value: PlainLines<'_>) -> Self {
        Stack::horizontal(
            value
                .text
                .lines()
                .map(|line| StackItem::auto(RawPrint::plain(line).styled(value.style))),
        )
        .into()
    }
}

#[derive(Clone, Debug)]
pub struct RawPrint<D = String> {
    pub raw: D,
    pub size: Vec2<u16>,
}
impl<D> RawPrint<D> {
    pub fn plain(text: D) -> Self
    where
        D: AsRef<str>,
    {
        // FIXME: Strip control characters
        let s = text.as_ref();
        if s.chars().any(|c| c.is_ascii_control()) {
            log::warn!("Plain text {s:?} should not contain ascii control chars");
        }
        Self {
            size: Vec2 {
                x: unicode_width::UnicodeWidthStr::width(s)
                    .try_into()
                    .unwrap_or(u16::MAX),
                y: 1,
            },
            raw: text,
        }
    }

    pub fn center_symbol(sym: D, width: u16) -> RawPrint<impl fmt::Display>
    where
        D: fmt::Display,
    {
        RawPrint {
            raw: KittyTextSize::center_width(width).apply(sym),
            size: Vec2 { x: width, y: 1 },
        }
    }

    pub fn styled(self, style: Style) -> RawPrint<impl fmt::Display>
    where
        D: fmt::Display,
    {
        let Self { size, raw } = self;
        RawPrint {
            size,
            raw: style.apply(raw),
        }
    }

    pub fn map_display<T>(self, f: impl FnOnce(D) -> T) -> RawPrint<T> {
        RawPrint {
            raw: f(self.raw),
            size: self.size,
        }
    }
}

#[derive(Default, Debug)]
pub struct PlainLines<'a> {
    pub text: &'a str,
    pub style: Style,
}
impl<'a> PlainLines<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            text,
            style: Default::default(),
        }
    }
    pub fn styled(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

#[derive(Clone)]
pub struct InteractCallback(Arc<dyn Fn(InteractData) + 'static + Send + Sync>);
impl fmt::Debug for InteractCallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&std::any::Any::type_id(&self.0), f)
    }
}
impl InteractCallback {
    pub fn from_fn(callback: impl Fn(InteractData) + 'static + Send + Sync) -> Self {
        Self(Arc::new(callback))
    }
    pub fn from_fn_ctx<C: 'static + Send + Sync>(
        ctx: C,
        callback: impl Fn(&C, InteractData) + 'static + Send + Sync,
    ) -> Self {
        Self::from_fn(move |it| callback(&ctx, it))
    }
}

pub struct InteractData {
    pub location: Vec2<u32>,
    pub monitor: Arc<str>,
    pub kind: InteractKind,
    _p: (),
}

#[derive(Clone, Copy, Debug)]
pub enum ImageSizeMode {
    FillAxis(Axis, u16),
}
#[derive(Clone)]
pub struct Image {
    pub img: image::RgbaImage,
    pub sizing: ImageSizeMode,
}

impl fmt::Debug for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
    pub inner: Option<Elem>,
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
        Self::length(len, Elem::empty())
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

#[derive(Default, Debug, Clone, Copy)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub modifier: Modifier,
    pub underline_color: Option<Color>,

    #[doc(hidden)]
    pub __non_exhaustive: (),
}
pub type Color = crossterm::style::Color;
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
#[repr(Rust, packed)]
pub struct Modifier {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub hidden: bool,
    pub strike: bool,
}

// TODO: enums
#[derive(Default, Debug, Clone, Copy)]
pub struct KittyTextSize {
    pub s: Option<u16>,
    pub w: Option<u16>,
    pub n: Option<u16>,
    pub d: Option<u16>,
    pub v: Option<u16>,
    pub h: Option<u16>,
}
impl KittyTextSize {
    pub fn center_width(width: u16) -> Self {
        // https://sw.kovidgoyal.net/kitty/text-sizing-protocol/
        // - w      sets the width of the multicell
        // - h      ceter the text horizontally
        // - n,d    use fractional scale of 1:1. kitty ignores w without this
        Self {
            w: Some(width),
            h: Some(2),
            d: Some(1),
            n: Some(1),
            ..Default::default()
        }
    }
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

macro_rules! lazy_str {
    ($s:expr) => {{
        static VALUE: std::sync::OnceLock<std::sync::Arc<str>> = std::sync::OnceLock::new();
        VALUE
            .get_or_init(|| std::sync::Arc::<str>::from($s))
            .clone()
    }};
}

impl LineSet {
    pub fn normal() -> Self {
        Self {
            vertical: lazy_str!("│"),
            horizontal: lazy_str!("─"),
            top_right: lazy_str!("┐"),
            top_left: lazy_str!("┌"),
            bottom_right: lazy_str!("┘"),
            bottom_left: lazy_str!("└"),
        }
    }

    #[expect(dead_code)]
    pub fn rounded() -> Self {
        Self {
            top_right: lazy_str!("╮"),
            top_left: lazy_str!("╭"),
            bottom_right: lazy_str!("╯"),
            bottom_left: lazy_str!("╰"),
            ..Self::normal()
        }
    }

    #[expect(dead_code)]
    pub fn double() -> Self {
        Self {
            vertical: lazy_str!("║"),
            horizontal: lazy_str!("═"),
            top_right: lazy_str!("╗"),
            top_left: lazy_str!("╔"),
            bottom_right: lazy_str!("╝"),
            bottom_left: lazy_str!("╚"),
        }
    }

    pub fn thick() -> Self {
        Self {
            vertical: lazy_str!("┃"),
            horizontal: lazy_str!("━"),
            top_right: lazy_str!("┓"),
            top_left: lazy_str!("┏"),
            bottom_right: lazy_str!("┛"),
            bottom_left: lazy_str!("┗"),
        }
    }

    #[expect(dead_code)]
    pub fn light_double_dashed() -> Self {
        Self {
            vertical: lazy_str!("╎"),
            horizontal: lazy_str!("╌"),
            ..Self::normal()
        }
    }

    #[expect(dead_code)]
    pub fn heavy_double_dashed() -> Self {
        Self {
            vertical: lazy_str!("╏"),
            horizontal: lazy_str!("╍"),
            ..Self::thick()
        }
    }

    #[expect(dead_code)]
    pub fn light_triple_dashed() -> Self {
        Self {
            vertical: lazy_str!("┆"),
            horizontal: lazy_str!("┄"),
            ..Self::normal()
        }
    }

    #[expect(dead_code)]
    pub fn heavy_triple_dashed() -> Self {
        Self {
            vertical: lazy_str!("┇"),
            horizontal: lazy_str!("┅"),
            ..Self::thick()
        }
    }

    #[expect(dead_code)]
    pub fn light_quadruple_dashed() -> Self {
        Self {
            vertical: lazy_str!("┊"),
            horizontal: lazy_str!("┈"),
            ..Self::normal()
        }
    }

    #[expect(dead_code)]
    pub fn heavy_quadruple_dashed() -> Self {
        Self {
            vertical: lazy_str!("┋"),
            horizontal: lazy_str!("┉"),
            ..Self::thick()
        }
    }
}
