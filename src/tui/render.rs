use std::io::Write;

use crate::tui::*;

#[derive(Clone, Copy, Debug)]
pub struct SizingContext {
    pub font_size: Vec2<u16>,
    pub div_w: Option<u16>,
    pub div_h: Option<u16>,
}
pub struct RenderCtx<'a, W> {
    pub writer: &'a mut W,
    pub layout: &'a mut RenderedLayout,
}

impl Tui {
    pub fn calc_size(&mut self, sizing: SizingContext) -> anyhow::Result<Vec2<u16>> {
        self.root.calc_auto_size(sizing)
    }
    pub fn render(
        &mut self,
        ctx: &mut RenderCtx<impl Write>,
        sizing: SizingContext,
        area: Area,
    ) -> std::io::Result<()> {
        self.root.render(ctx, sizing, area)
    }
}
impl Elem {
    // TODO: Pass constraint and axis to size calc method, generalize, split auto into Fit, Fill
    pub fn calc_auto_size(&mut self, sizing: SizingContext) -> anyhow::Result<Vec2<u16>> {
        auto_size_invariants(sizing, || match self {
            Self::Stack(subdiv) => subdiv.calc_auto_size(sizing),
            Self::Text(text) => text.calc_auto_size(sizing),
            Self::Image(image) => image.calc_auto_size(sizing),
            Self::Block(block) => block.calc_auto_size(sizing),
            Self::Tagged(elem) => elem.elem.calc_auto_size(sizing),
            Self::Empty => Ok(Vec2::default()),
        })
    }
    pub fn render(
        &mut self,
        ctx: &mut RenderCtx<impl Write>,
        sizing: SizingContext,
        area: Area,
    ) -> std::io::Result<()> {
        match self {
            Self::Stack(subdiv) => subdiv.render(ctx, sizing, area),
            Self::Image(image) => image.render(ctx, sizing, area),
            Self::Block(block) => block.render(ctx, sizing, area),
            Self::Text(text) => text.render(ctx, sizing, area),
            Self::Tagged(elem) => {
                ctx.layout.insert(area, elem.tag.clone());
                elem.elem.render(ctx, sizing, area)
            }
            Self::Empty => Ok(()),
        }
    }
}
impl Image {
    // Computes the maximal dimensions with correct aspect ratio that do not exceed
    // (div_w, div_h), if specified. Errors if neither is specified.
    //
    // Also returns the axis that is being filled.
    fn calc_fill_size(&mut self, sizing: SizingContext) -> anyhow::Result<(Axis, Vec2<u16>)> {
        let Vec2 {
            x: font_w,
            y: font_h,
        } = sizing.font_size;

        let img = self.load()?;
        // Aspect ratio of the image in cells
        let cell_ratio = std::ops::Mul::mul(
            f64::from(img.width()) / f64::from(img.height()),
            f64::from(font_h) / f64::from(font_w),
        );

        let (fill_axis, fill_axis_len) = match (sizing.div_w, sizing.div_h) {
            // larger aspect ratio means wider.
            // if the aspect ratio of the bounding box is wider than that of the image,
            // it is effectively unconstrained along the horizontal axis. That makes
            // it the flex axis, the other the fill axis.
            (Some(w), Some(h)) => match f64::from(w) / f64::from(h) > cell_ratio {
                true => (Axis::Y, h),
                false => (Axis::X, w),
            },
            (None, Some(h)) => (Axis::Y, h),
            (Some(w), None) => (Axis::X, w),
            (None, None) => anyhow::bail!("sizing context must include some dimension to fill"),
        };

        Ok((
            fill_axis,
            match fill_axis {
                Axis::Y => Vec2 {
                    y: fill_axis_len,
                    // cell ratio is width over height, so we get the flex dimension by multiplying
                    x: (cell_ratio * f64::from(fill_axis_len)).ceil() as _,
                },
                Axis::X => Vec2 {
                    x: fill_axis_len,
                    // likewise, but by division
                    y: (cell_ratio / f64::from(fill_axis_len)).ceil() as _,
                },
            },
        ))
    }
    pub fn calc_auto_size(&mut self, sizing: SizingContext) -> anyhow::Result<Vec2<u16>> {
        auto_size_invariants(sizing, || {
            Ok(match (sizing.div_w, sizing.div_h) {
                (Some(w), Some(h)) => Vec2 { x: w, y: h },
                _ => self.calc_fill_size(sizing)?.1,
            })
        })
    }
    fn render(
        &mut self,
        ctx: &mut RenderCtx<impl Write>,
        sizing: SizingContext,
        area: Area,
    ) -> std::io::Result<()> {
        let Ok((fill_axis, fill_size)) = self
            .calc_fill_size(sizing)
            .map_err(|err| log::error!("{err}"))
        else {
            return Ok(());
        };
        let Ok(img) = self.load().map_err(|err| log::error!("{err}")) else {
            return Ok(());
        };

        crossterm::queue!(
            ctx.writer,
            crossterm::cursor::MoveTo(area.pos.x, area.pos.y),
        )?;

        // https://sw.kovidgoyal.net/kitty/graphics-protocol/#control-data-reference
        // Explanation:
        // - \x1b_G...\x1b\\: kitty graphics apc
        // - a=T: Transfer and display
        // - f=32: 32-bit RGBA
        // - C=1: Do not move the cursor behind the image after drawing. If the image is on the
        //   last line, the first line would move to scrollback (effectively a clear if there is
        //   only one line, like in the bar).
        // - s and v specify the image's dimensions
        write!(
            ctx.writer,
            "\x1b_Ga=T,f=32,C=1,s={},v={},{}={};",
            img.width(),
            img.height(),
            match fill_axis {
                Axis::X => "c",
                Axis::Y => "r",
            },
            fill_size.get(fill_axis),
        )?;
        {
            let mut encoder_writer = base64::write::EncoderWriter::new(
                &mut ctx.writer,
                &base64::engine::general_purpose::STANDARD,
            );
            encoder_writer.write_all(img.as_raw())?;
        }
        write!(ctx.writer, "\x1b\\")?;

        Ok(())
    }
}
impl Stack {
    fn calc_elem_auto_size(
        part: &mut StackItem,
        sizing: SizingContext,
        axis: Axis,
    ) -> anyhow::Result<Vec2<u16>> {
        part.elem
            .calc_auto_size(Self::inner_sizing_arg(&part.constr, sizing, axis))
    }
    fn inner_sizing_arg(constr: &Constr, sizing: SizingContext, axis: Axis) -> SizingContext {
        match *constr {
            Constr::Length(l) => match axis {
                Axis::X => SizingContext {
                    div_w: Some(l),
                    ..sizing
                },
                Axis::Y => SizingContext {
                    div_h: Some(l),
                    ..sizing
                },
            },
            Constr::Fill(_) | Constr::Auto => match axis {
                Axis::X => SizingContext {
                    div_w: None,
                    ..sizing
                },
                Axis::Y => SizingContext {
                    div_h: None,
                    ..sizing
                },
            },
        }
    }
    pub fn calc_auto_size(&mut self, sizing: SizingContext) -> anyhow::Result<Vec2<u16>> {
        auto_size_invariants(sizing, || {
            let mut size = Vec2::default();
            for part in &mut self.parts {
                let elem_size = Self::calc_elem_auto_size(part, sizing, self.axis)?;
                let horiz = (&mut size.x, elem_size.x);
                let vert = (&mut size.y, elem_size.y);
                let ((adst, asrc), (mdst, msrc)) = match self.axis {
                    Axis::X => (horiz, vert),
                    Axis::Y => (vert, horiz),
                };
                *adst += asrc;
                *mdst = msrc.max(*mdst);
            }
            Ok(size)
        })
    }
    fn render(
        &mut self,
        ctx: &mut RenderCtx<impl Write>,
        sizing: SizingContext,
        area: Area,
    ) -> std::io::Result<()> {
        let mut lens = Vec::with_capacity(self.parts.len());
        let mut total_weight = 0u64;
        let mut rem_len = Some(area.size.get(self.axis));
        for part in &mut self.parts {
            let len = match part.constr {
                Constr::Length(len) => len,
                Constr::Auto => Self::calc_elem_auto_size(part, sizing, self.axis)
                    .unwrap_or_else(|err| {
                        log::error!("Skipping element {part:?} with broken size: {err}");
                        Default::default()
                    })
                    .get(self.axis),
                Constr::Fill(weight) => {
                    total_weight += u64::from(weight);
                    0
                }
            };
            if let Some(rlen) = rem_len {
                rem_len = rlen.checked_sub(len);
            }
            lens.push(len)
        }
        assert_eq!(lens.len(), self.parts.len());

        let fill_len = rem_len.unwrap_or_else(|| {
            log::warn!("Stack does not fit into {area:?}: {self:?}");
            0
        });

        if total_weight > 0 {
            let mut rem_fill_len = fill_len;

            for (part, len) in self.parts.iter_mut().zip(&mut lens) {
                if let Constr::Fill(weight) = part.constr {
                    // weight does not exceed total weight, so this should always succeed
                    *len = u16::try_from(u64::from(fill_len) * u64::from(weight) / total_weight)
                        .unwrap();
                    rem_fill_len = rem_fill_len.checked_sub(*len).unwrap();
                }
            }
            if rem_fill_len > 0 {
                let mut fills: Vec<_> = self
                    .parts
                    .iter_mut()
                    .zip(&mut lens)
                    .filter_map(|(part, len)| match part.constr {
                        Constr::Fill(weight) if weight > 0 => Some((weight, len)),
                        _ => None,
                    })
                    .collect();
                fills.sort();
                for (_, len) in fills.into_iter().take(rem_fill_len.into()) {
                    *len += 1;
                }
            }
        }

        let mut offset = 0;
        for (part, len) in self.parts.iter_mut().zip(lens) {
            let mut subarea = area;
            *subarea.size.get_mut(self.axis) = len;
            *subarea.pos.get_mut(self.axis) += offset;

            part.elem.render(
                ctx,
                Self::inner_sizing_arg(&part.constr, sizing, self.axis),
                subarea,
            )?;

            offset += len;
        }

        Ok(())
    }
}
// TODO: Styling (using crossterm)
impl Text {
    pub fn calc_auto_size(&mut self, sizing: SizingContext) -> anyhow::Result<Vec2<u16>> {
        // TODO: Warn if text is too large
        auto_size_invariants(sizing, || {
            Ok(Vec2 {
                x: self.width,
                y: self.lines.iter().map(|line| line.height).sum(),
            })
        })
    }
    fn render(
        &mut self,
        ctx: &mut RenderCtx<impl Write>,
        _: SizingContext,
        area: Area,
    ) -> std::io::Result<()> {
        let mut y_off = 0;
        // TODO: Style
        for line in &self.lines {
            let Some(y) = area.pos.y.checked_add(y_off) else {
                log::error!("Vertical position overflow");
                break;
            };
            crossterm::queue!(
                ctx.writer,
                crossterm::cursor::MoveTo(area.pos.x, y),
                crossterm::style::Print(stylize(line.text.as_str(), self.style)),
            )?;
            let Some(new_y_off) = y_off.checked_add(line.height) else {
                log::error!("Vertical position overflow");
                break;
            };
            y_off = new_y_off;
        }

        Ok(())
    }
}
impl Block {
    fn extra_dim(&self) -> Vec2<u16> {
        let Borders {
            top,
            bottom,
            left,
            right,
        } = self.borders;
        Vec2 {
            x: u16::from(left) + u16::from(right),
            y: u16::from(top) + u16::from(bottom),
        }
    }
    fn inner_sizing_arg(&self, mut sizing: SizingContext) -> SizingContext {
        let Vec2 { x: w, y: h } = self.extra_dim();
        if let Some(div_w) = &mut sizing.div_w {
            *div_w -= w;
        }
        if let Some(div_h) = &mut sizing.div_h {
            *div_h -= h;
        }
        sizing
    }
    pub fn calc_auto_size(&mut self, sizing: SizingContext) -> anyhow::Result<Vec2<u16>> {
        auto_size_invariants(sizing, || {
            let inner_ctx = self.inner_sizing_arg(sizing);
            let mut size = self
                .inner
                .as_mut()
                .map(|it| it.calc_auto_size(inner_ctx))
                .transpose()?
                .unwrap_or_default();
            let Vec2 { x: w, y: h } = self.extra_dim();
            size.x = size.x.saturating_add(w);
            size.y = size.y.saturating_add(h);
            Ok(size)
        })
    }
    fn render(
        &mut self,
        ctx: &mut RenderCtx<impl Write>,
        sizing: SizingContext,
        area: Area,
    ) -> std::io::Result<()> {
        let Borders {
            top,
            bottom,
            left,
            right,
        } = self.borders;

        let inner_sizing_arg = self.inner_sizing_arg(sizing);
        if let Some(inner) = &mut self.inner {
            let Area {
                pos: Vec2 { x, y },
                size: Vec2 { x: w, y: h },
            } = area;
            inner.render(
                ctx,
                inner_sizing_arg,
                Area {
                    pos: Vec2 {
                        x: x.saturating_add(left.into()),
                        y: y.saturating_add(top.into()),
                    },
                    size: Vec2 {
                        x: w.saturating_sub(right.into()),
                        y: h.saturating_sub(bottom.into()),
                    },
                },
            )?;
        }

        let mut horiz_border = |l: &str, r: &str, y: u16| {
            let m = stylize(
                self.border_set.horizontal.repeat(
                    area.size
                        .x
                        .saturating_sub(left.into())
                        .saturating_sub(right.into())
                        .into(),
                ),
                self.border_style,
            );
            let l = stylize(if left { l } else { "" }, self.border_style);
            let r = stylize(if right { r } else { "" }, self.border_style);

            crossterm::queue!(
                ctx.writer,
                crossterm::cursor::MoveTo(area.pos.x, y),
                crossterm::style::Print(format_args!("{l}{m}{r}")),
            )
        };
        if top {
            horiz_border(
                &self.border_set.top_left,
                &self.border_set.top_right,
                area.pos.y,
            )?;
        }
        if bottom {
            horiz_border(
                &self.border_set.bottom_left,
                &self.border_set.bottom_right,
                area.y_bottom(),
            )?;
        }
        let mut vert_border = |x: u16| -> std::io::Result<()> {
            let lo = area.pos.y.saturating_add(top.into());
            let hi = area
                .pos
                .y
                .saturating_add(area.size.y)
                .saturating_sub(bottom.into());
            for y in lo..hi {
                crossterm::queue!(
                    ctx.writer,
                    crossterm::cursor::MoveTo(x, y),
                    crossterm::style::Print(stylize(
                        &self.border_set.vertical as &str,
                        self.border_style
                    )),
                )?;
            }
            Ok(())
        };
        if left {
            vert_border(area.pos.x)?;
        }
        if right {
            vert_border(area.x_right())?;
        }

        Ok(())
    }
}
fn auto_size_invariants(
    sizing: SizingContext,
    f: impl FnOnce() -> anyhow::Result<Vec2<u16>>,
) -> anyhow::Result<Vec2<u16>> {
    if let (Some(w), Some(h)) = (sizing.div_w, sizing.div_h) {
        return Ok(Vec2 { x: w, y: h });
    }
    let mut size = f()?;
    if let Some(w) = sizing.div_w {
        size.x = w;
    }
    if let Some(h) = sizing.div_h {
        size.y = h;
    }
    Ok(size)
}
fn stylize<S>(
    s: S,
    Style {
        fg,
        bg,
        modifier:
            Modifier {
                bold,
                dim,
                italic,
                underline,
                hidden,
                strike,
            },
        underline_color,
    }: Style,
) -> S::Styled
where
    S: crossterm::style::Stylize<
            Styled: crossterm::style::Stylize<Styled = S::Styled> + std::fmt::Display,
        >,
{
    use crossterm::style::Stylize;

    let mut s = s.stylize();
    if bold {
        s = s.bold();
    }
    if dim {
        s = s.dim();
    }
    if italic {
        s = s.italic();
    }
    if underline {
        s = s.underlined();
    }
    if hidden {
        s = s.hidden();
    }
    if strike {
        s = s.crossed_out();
    }
    if let Some(fg) = fg {
        s = s.with(fg);
    }
    if let Some(bg) = bg {
        s = s.on(bg);
    }
    if let Some(col) = underline_color {
        s = s.underline(col);
    }

    s
}

pub fn draw_to<W: std::io::Write>(
    writer: &mut W,
    doit: impl FnOnce(&mut RenderCtx<W>) -> std::io::Result<()>,
) -> std::io::Result<RenderedLayout> {
    let mut layout = Default::default();
    let mut ctx = RenderCtx {
        layout: &mut layout,
        writer,
    };
    crossterm::queue!(
        ctx.writer,
        crossterm::terminal::BeginSynchronizedUpdate,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
    )?;
    doit(&mut ctx)?;
    crossterm::execute!(ctx.writer, crossterm::terminal::EndSynchronizedUpdate)?;
    Ok(layout)
}
