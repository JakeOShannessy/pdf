#[macro_use] extern crate log;
#[macro_use] extern crate pdf;

use std::convert::TryInto;
use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::fs;
use std::borrow::Cow;
use std::sync::Arc;

use pdf::file::File as PdfFile;
use pdf::object::*;
use pdf::primitive::Primitive;
use pdf::backend::Backend;
use pdf::font::{Font as PdfFont, FontType};
use pdf::error::{PdfError, Result};
use pdf::encoding::{Encoding as PdfEncoding, BaseEncoding};
use pdf::content::Operation;
use encoding::{Encoding};

use pathfinder_geometry::{
    vector::{Vector2F, Vector2I},
    rect::RectF, transform2d::Transform2F,
};
use pathfinder_content::{
    fill::FillRule,
    stroke::{LineCap, LineJoin, StrokeStyle, OutlineStrokeToFill},
    outline::Outline,
    pattern::{Pattern, Image},
};
use pathfinder_color::ColorU;
use pathfinder_renderer::{
    scene::{DrawPath, ClipPath, ClipPathId},
    paint::{Paint, PaintId},
    scene::Scene,
};
use pathfinder_builder::PathBuilder;

use font::{self, Font, GlyphId};

macro_rules! ops_p {
    ($ops:ident, $($point:ident),* => $block:block) => ({
        let mut iter = $ops.iter();
        $(
            let x = iter.next().unwrap().as_number().unwrap();
            let y = iter.next().unwrap().as_number().unwrap();
            let $point = Vector2F::new(x, y);
        )*
        $block
    })
}
macro_rules! ops {
    ($ops:ident, $($var:ident : $typ:ty),* => $block:block) => ({
        let mut iter = $ops.iter();
        $(
            let $var: $typ = iter.next().ok_or(PdfError::EOF)?.try_into()?;
        )*
        $block
    })
}

fn rgb2fill(r: f32, g: f32, b: f32) -> Paint {
    let c = |v: f32| (v * 255.) as u8;
    Paint::from_color(ColorU::new(c(r), c(g), c(b), 255))
}
fn gray2fill(g: f32) -> Paint {
    rgb2fill(g, g, g)
}
fn cmyk2fill(c: f32, m: f32, y: f32, k: f32) -> Paint {
    rgb2fill(
        (1.0 - c) * (1.0 - k),
        (1.0 - m) * (1.0 - k),
        (1.0 - y) * (1.0 - k)
    )
}

#[derive(Copy, Clone)]
struct BBox(Option<RectF>);
impl BBox {
    fn empty() -> Self {
        BBox(None)
    }
    fn add(&mut self, r2: RectF) {
        self.0 = Some(match self.0 {
            Some(r1) => r1.union_rect(r2),
            None => r2
        });
    }
    fn add_bbox(&mut self, bb: Self) {
        if let Some(r) = bb.0 {
            self.add(r);
        }
    }
    fn rect(self) -> Option<RectF> {
        self.0
    }
}

#[derive(Debug)]
enum TextEncoding {
    CID,
    Cmap(HashMap<u16, GlyphId>)
}

struct FontEntry {
    font: Box<dyn Font>,
    encoding: TextEncoding,
    is_cid: bool,
}
#[derive(Copy, Clone)]
enum TextMode {
    Fill,
    Stroke,
    FillThenStroke,
    Invisible,
    FillAndClip,
    StrokeAndClip
}

#[derive(Copy, Clone)]
struct GraphicsState<'a> {
    transform: Transform2F,
    stroke_width: f32,
    fill_paint: PaintId,
    stroke_paint: PaintId,
    clip_path: Option<ClipPathId>,
    fill_color_space: &'a ColorSpace,
    stroke_color_space: &'a ColorSpace,
}

#[derive(Copy, Clone)]
enum DrawMode {
    None,
    Fill(PaintId),
    Stroke(PaintId, StrokeStyle),
    StrokeThenFill(PaintId, StrokeStyle, PaintId),
    FillThenStroke(PaintId, PaintId, StrokeStyle)
}

struct PathStyle {
    mode: DrawMode,
    fill_rule: FillRule
}
fn draw(scene: &mut Scene, path: Outline, style: &PathStyle, clip: Option<ClipPathId>) {
    let build_stroke = |path, paint, stroke| {
        let mut stroke_to_fill = OutlineStrokeToFill::new(path, stroke);
        stroke_to_fill.offset();
        let outline = stroke_to_fill.into_outline();
        let mut draw_path = DrawPath::new(outline, paint);
        draw_path.set_fill_rule(style.fill_rule);
        draw_path.set_clip_path(clip);
        draw_path
    };
    let build_fill = |path, paint| {
        let mut draw_path = DrawPath::new(path, paint);
        draw_path.set_fill_rule(style.fill_rule);
        draw_path.set_clip_path(clip);
        draw_path
    };
    
    match style.mode {
        DrawMode::None => {},
        DrawMode::Fill(paint) => {
            scene.push_path(build_fill(path, paint));
        }
        DrawMode::Stroke(paint, stroke) => {
            scene.push_path(build_stroke(&path, paint, stroke));
        }
        DrawMode::FillThenStroke(fill_paint, stroke_paint, stroke) => {
            scene.push_path(build_fill(path.clone(), fill_paint));
            scene.push_path(build_stroke(&path, stroke_paint, stroke));
        }
        DrawMode::StrokeThenFill(fill_paint, stroke, stroke_paint) => {
            scene.push_path(build_stroke(&path, stroke_paint, stroke));
            scene.push_path(build_fill(path, fill_paint));
        }
    }
}

impl<'a> GraphicsState<'a> {
    fn get_text_style(&self, mode: TextMode) -> PathStyle {
        match mode {
            TextMode::Fill => self.fill_style(FillRule::Winding),
            TextMode::Stroke => self.stroke_style(),
            TextMode::FillThenStroke => self.fill_then_stroke_style(FillRule::Winding),
            _ => PathStyle {
                mode: DrawMode::None,
                fill_rule: FillRule::Winding,
            }
        }
    }
    fn line_style(&self) -> StrokeStyle {
        let line_width = self.stroke_width * self.transform.matrix.m11();
        StrokeStyle {
            line_cap: LineCap::Butt,
            line_join: LineJoin::Miter(line_width),
            line_width
        }
    }
    fn fill_style(&self, fill_rule: FillRule) -> PathStyle {
        PathStyle {
            mode: DrawMode::Fill(
                self.fill_paint,
            ),
            fill_rule,
        }
    }
    fn stroke_style(&self) -> PathStyle {
        PathStyle {
            mode: DrawMode::Stroke(
                self.stroke_paint,
                self.line_style()
            ),
            fill_rule: FillRule::Winding,
        }
    }
    fn fill_then_stroke_style(&self, fill_rule: FillRule) -> PathStyle {
        PathStyle {
            mode: DrawMode::FillThenStroke(
                self.fill_paint,
                self.stroke_paint,
                self.line_style(),
            ),
            fill_rule,
        }
    }
    fn stroke_then_fill_style(&self, fill_rule: FillRule) -> PathStyle {
        PathStyle {
            mode: DrawMode::StrokeThenFill(
                self.stroke_paint,
                self.line_style(),
                self.fill_paint,
            ),
            fill_rule,
        }
    }
}

#[derive(Copy, Clone)]
struct TextState<'a> {
    text_matrix: Transform2F, // tracks current glyph
    line_matrix: Transform2F, // tracks current line
    char_space: f32, // Character spacing
    word_space: f32, // Word spacing
    horiz_scale: f32, // Horizontal scaling
    leading: f32, // Leading
    font_entry: Option<&'a FontEntry>, // Text font
    font_size: f32, // Text font size
    mode: TextMode, // Text rendering mode
    rise: f32, // Text rise
    knockout: f32, //Text knockout
}
impl<'a> TextState<'a> {
    fn new() -> TextState<'a> {
        TextState {
            text_matrix: Transform2F::default(),
            line_matrix: Transform2F::default(),
            char_space: 0.,
            word_space: 0.,
            horiz_scale: 1.,
            leading: 0.,
            font_entry: None,
            font_size: 0.,
            mode: TextMode::Fill,
            rise: 0.,
            knockout: 0.
        }
    }
    fn reset_matrix(&mut self) {
        self.set_matrix(Transform2F::default());
    }
    fn translate(&mut self, v: Vector2F) {
        let m = self.line_matrix * Transform2F::from_translation(v);
        self.set_matrix(m);
    }
    
    // move to the next line
    fn next_line(&mut self) {
        self.translate(Vector2F::new(0., -self.leading * self.font_size));
    }
    // set text and line matrix
    fn set_matrix(&mut self, m: Transform2F) {
        self.text_matrix = m;
        self.line_matrix = m;
    }
    fn add_glyphs(&mut self, root_tr: Transform2F, mut draw: impl FnMut(Outline), glyphs: impl Iterator<Item=(u16, Option<GlyphId>)>) -> BBox {
        let e = self.font_entry.as_ref().expect("no font");
        let mut bbox = BBox::empty();

        let tr = Transform2F::row_major(
            self.horiz_scale * self.font_size, 0., 0.,
            0., self.font_size, self.rise
        ) * e.font.font_matrix();
        
        let mut text = String::with_capacity(32);
        for (cid, gid) in glyphs {
            if let Some(c) = std::char::from_u32(cid as u32) {
                text.push(c);
            }
            debug!("cid {} -> gid {:?}", cid, gid);
            let gid = match gid {
                Some(gid) => gid,
                None => {
                    warn!("no glyph for cid {}", cid);
                    GlyphId(0)
                } // lets hope that works…
            };
            if let Some(glyph) = e.font.glyph(gid) {
                let transform = root_tr * self.text_matrix * tr;
                let path = glyph.path.transformed(&transform);
                if path.len() != 0 {
                    bbox.add(path.bounds());
                    draw(path);
                }
                
                let dx = match cid {
                    0x20 => self.word_space,
                    _ => self.char_space
                };
                let advance = dx * self.horiz_scale * self.font_size + tr.m11() * glyph.metrics.advance.x();
                self.text_matrix = self.text_matrix * Transform2F::from_translation(Vector2F::new(advance, 0.));
            } else {
                info!("no glyph for gid {:?}", gid);
            }
        }
        debug!("text: {}", text);
        bbox
    }
    fn draw_text(&mut self, root_tr: Transform2F, draw: impl FnMut(Outline), data: &[u8]) -> BBox {
        debug!("text: {:?}", String::from_utf8_lossy(data));
        if let Some(e) = self.font_entry {
            let get_glyph = |cid: u16| {
                let gid = match e.encoding {
                    TextEncoding::CID => Some(GlyphId(cid as u32)),
                    TextEncoding::Cmap(ref cmap) => cmap.get(&cid).cloned()
                };
                (cid, gid)
            };
            if e.is_cid {
                self.add_glyphs(
                    root_tr,
                    draw,
                    data.chunks_exact(2).map(|s| get_glyph(u16::from_be_bytes(s.try_into().unwrap()))),
                )
            } else {
                self.add_glyphs(
                    root_tr,
                    draw,
                    data.iter().map(|&b| get_glyph(b as u16))
                )
            }
        } else {
            warn!("no font set");
            BBox::empty()
        }
    }
    fn advance(&mut self, delta: f32) {
        //debug!("advance by {}", delta);
        let advance = delta * self.font_size * self.horiz_scale;
        self.text_matrix = self.text_matrix * Transform2F::from_translation(Vector2F::new(advance, 0.));
    }
}

pub struct Cache {
    // shared mapping of fontname -> font
    fonts: HashMap<String, FontEntry>
}
impl FontEntry {
    fn build(font: Box<dyn Font>, pdf_font: &PdfFont) -> FontEntry {
        let mut is_cid = pdf_font.is_cid();
        let encoding = pdf_font.encoding().clone();
        let base_encoding = encoding.as_ref().map(|e| &e.base);

        let encoding = if let Some(map) = pdf_font.cid_to_gid_map() {
            is_cid = true;
            let cmap = map.iter().enumerate().map(|(cid, &gid)| (cid as u16, GlyphId(gid as u32))).collect();
            TextEncoding::Cmap(cmap)
        } else if base_encoding == Some(&BaseEncoding::IdentityH) {
            is_cid = true;
            TextEncoding::CID
        } else {
            let mut cmap = HashMap::new();
            let source_encoding = match base_encoding {
                Some(BaseEncoding::StandardEncoding) => Some(Encoding::AdobeStandard),
                Some(BaseEncoding::SymbolEncoding) => Some(Encoding::AdobeSymbol),
                Some(BaseEncoding::WinAnsiEncoding) => Some(Encoding::WinAnsiEncoding),
                ref e => {
                    warn!("unsupported pdf encoding {:?}", e);
                    None
                }
            };
            let font_encoding = font.encoding();
            debug!("{:?} -> {:?}", source_encoding, font_encoding);
            match (source_encoding, font_encoding) {
                (Some(source), Some(dest)) => {
                    let transcoder = source.to(dest).expect("can't transcode");
                    
                    for b in 0 .. 256 {
                        if let Some(gid) = transcoder.translate(b).and_then(|cp| font.gid_for_codepoint(cp)) {
                            cmap.insert(b as u16, gid);
                            debug!("{} -> {:?}", b, gid);
                        }
                    }
                },
                _ => {
                    warn!("can't translate from text encoding {:?} to font encoding {:?}", base_encoding, font_encoding);
                    
                    // assuming same encoding
                    for cp in 0 .. 256 {
                        if let Some(gid) = font.gid_for_codepoint(cp) {
                            cmap.insert(cp as u16, gid);
                        }
                    }
                }
            }
            if let Some(encoding) = encoding {
                for (&cp, name) in encoding.differences.iter() {
                    debug!("{} -> {}", cp, name);
                    match font.gid_for_name(&name) {
                        Some(gid) => {
                            cmap.insert(cp as u16, gid);
                        }
                        None => info!("no glyph for name {}", name)
                    }
                }
            }
            debug!("cmap: {:?}", cmap);
            if cmap.is_empty() {
                TextEncoding::CID
            } else {
                TextEncoding::Cmap(cmap)
            }
        };
        
        FontEntry {
            font: font,
            encoding,
            is_cid,
        }
    }
}

pub struct ItemMap(Vec<(RectF, Operation)>);
impl ItemMap {
    pub fn print(&self, p: Vector2F) {
        for &(rect, ref op) in self.0.iter() {
            if rect.contains_point(p) {
                println!("{}", op);
            }
        }
    }
    pub fn get_string(&self, p: Vector2F) -> Option<String> {
        use itertools::Itertools;
        let mut iter = self.0.iter().filter_map(|&(rect, ref op)| {
            if rect.contains_point(p) {
                Some(op)
            } else {
                None
            }
        }).peekable();
        if iter.peek().is_some() {
            Some(iter.format(", ").to_string())
        } else {
            None
        }
    }
}

fn fill_rule(s: &str) -> FillRule {
    if s.ends_with("*") {
        FillRule::EvenOdd
    } else {
        FillRule::Winding
    }
}

fn convert_color(cs: &ColorSpace, ops: &[Primitive]) -> Result<Paint> {
    match *cs {
        ColorSpace::DeviceRGB | ColorSpace::Icc(_) => ops!(ops, r: f32, g: f32, b: f32 => {
            Ok(rgb2fill(r, g, b))
        }),
        ColorSpace::DeviceCMYK => ops!(ops, c: f32, m: f32, y: f32, k: f32 => {
            Ok(cmyk2fill(c, m, y, k))
        }),
        ColorSpace::Separation(_, _, ref f) => ops!(ops, x: f32 => {
            let mut rgb = [0.0, 0.0, 0.0];
            f.apply(x, &mut rgb);
            let [r, g, b] = rgb;
            Ok(rgb2fill(r, g, b))
        }),
        ColorSpace::Indexed(ref cs, ref lut) => ops!(ops, i: i32 => {
            match **cs {
                ColorSpace::DeviceRGB => {
                    let c = &lut[3 * i as usize ..];
                    Ok(Paint::from_color(ColorU::new(c[0], c[1], c[2], 255)))
                }
                ColorSpace::DeviceCMYK => {
                    let c = &lut[4 * i as usize ..];
                    let cvt = |b: u8| b as f32 * 255.;
                    Ok(cmyk2fill(cvt(c[0]), cvt(c[1]), cvt(c[2]), cvt(c[3])))
                }
                _ => unimplemented!()
            }
        }),
        _ => unimplemented!()
    }
}

impl Cache {
    pub fn new() -> Cache {
        Cache {
            fonts: HashMap::new()
        }
    }
    fn load_font(&mut self, pdf_font: &PdfFont) {
        if self.fonts.get(&pdf_font.name).is_some() {
            return;
        }
        
        debug!("loading {:?}", pdf_font);
        
        let data: Cow<[u8]> = match (pdf_font.standard_font(), pdf_font.embedded_data()) {
            (_, Some(Ok(data))) => {
                if let Some(path) = std::env::var_os("PDF_FONTS") {
                    let file = PathBuf::from(path).join(&pdf_font.name);
                    fs::write(file, data).expect("can't write font");
                }
                data.into()
            }
            (Some(data), _) => data.into(),
            (None, Some(Err(e))) => panic!("can't decode font data: {:?}", e),
            (None, None) => {
                info!("Font: {:?}", pdf_font);
                warn!("No font data for {}. Glyphs will be missing.", pdf_font.name);
                return;
            }
        };
        let entry = FontEntry::build(font::parse(&data), pdf_font);
        debug!("is_cid={}", entry.is_cid);
            
        self.fonts.insert(pdf_font.name.clone(), entry);
    }
    fn get_font(&self, font_name: &str) -> Option<&FontEntry> {
        self.fonts.get(font_name)
    }
    
    pub fn render_page<B: Backend>(&mut self, file: &PdfFile<B>, page: &Page) -> Result<(Scene, ItemMap)> {
        self.render_page_n(file, page, usize::max_value())
    }
    pub fn render_page_n<B: Backend>(&mut self, file: &PdfFile<B>, page: &Page, num_ops: usize) -> Result<(Scene, ItemMap)> {
        let Rect { left, right, top, bottom } = page.media_box(file).expect("no media box");
        let rect = RectF::from_points(Vector2F::new(left, bottom), Vector2F::new(right, top));
        
        let scale = Vector2F::splat(0.5);
        let mut scene = Scene::new();
        scene.set_view_box(RectF::new(Vector2F::default(), rect.size() * scale));
        
        let black = scene.push_paint(&Paint::from_color(ColorU::black()));
        let white = scene.push_paint(&Paint::from_color(ColorU::white()));

        let mut path_builder = PathBuilder::new();

        let mut items = Vec::new();
        let mut add_item = |bbox: BBox, op: &Operation| if let Some(r) = bbox.rect() {
            items.push((r, op.clone()));
        };

        // draw the page
        let style = PathStyle {
            mode: DrawMode::FillThenStroke(
                white,
                black,
                StrokeStyle {
                    line_cap: LineCap::Round,
                    line_join: LineJoin::Round,
                    line_width: 0.25
                },
            ),
            fill_rule: FillRule::Winding,
        };
        path_builder.rect(RectF::new(Vector2F::default(), rect.size() * scale));
        draw(&mut scene, path_builder.take(), &style, None);

        let root_transformation = Transform2F::from_scale(scale) * Transform2F::row_major(1.0, 0.0, -left, 0.0, -1.0, top);
        
        let resources = page.resources(file)?;
        // make sure all fonts are in the cache, so we can reference them
        for font in resources.fonts.values() {
            self.load_font(font);
        }
        for gs in resources.graphics_states.values() {
            if let Some((ref font, _)) = gs.font {
                self.load_font(font);
            }
        }

        let device_rgb = ColorSpace::DeviceRGB;
        
        let mut text_state = TextState::new();
        let mut stack = vec![];

        path_builder.move_to(Vector2F::default());
        let mut graphics_state = GraphicsState {
            transform: root_transformation,
            stroke_width: 1.0,
            fill_paint: black,
            stroke_paint: black,
            clip_path: None,
            fill_color_space: &device_rgb,
            stroke_color_space: &device_rgb,
        };
        
        let contents = try_opt!(page.contents.as_ref());
        
        for op in contents.operations.iter().take(num_ops) {
            debug!("{}", op);
            let ref ops = op.operands;
            let s = op.operator.as_str();
            match s {
                "m" => { // move x y
                    ops_p!(ops, p => {
                        path_builder.move_to(p);
                    })
                }
                "l" => { // line x y
                    ops_p!(ops, p => {
                        path_builder.line_to(p);
                    })
                }
                "c" => { // cubic bezier c1.x c1.y c2.x c2.y p.x p.y
                    ops_p!(ops, c1, c2, p => {
                        path_builder.cubic_curve_to(c1, c2, p);
                    })
                }
                "v" => { // cubic bezier c2.x c2.y p.x p.y
                    ops_p!(ops, c2, p => {
                        let last = path_builder.pos().unwrap();
                        path_builder.cubic_curve_to(last, c2, p);
                    })
                }
                "y" => { // cubic c1.x c1.y p.x p.y
                    ops_p!(ops, c1, p => {
                        path_builder.cubic_curve_to(c1, p, p);
                    })
                }
                "h" => { // close
                    path_builder.close();
                }
                "re" => { // rect x y width height
                    ops_p!(ops, origin, size => {
                        let r = RectF::new(origin, size);
                        path_builder.rect(r);
                    })
                }
                "S" => { // stroke
                    let style = graphics_state.stroke_style();
                    let path = path_builder.take().transformed(&graphics_state.transform);
                    draw(&mut scene, path, &style, graphics_state.clip_path);
                }
                "s" => { // close and stroke
                    path_builder.close();
                    let style = graphics_state.stroke_style();
                    let path = path_builder.take().transformed(&graphics_state.transform);
                    draw(&mut scene, path, &style, graphics_state.clip_path);
                }
                "f" | "F" | "f*" => { // close and fill 
                    // TODO: implement windings
                    path_builder.close();
                    let path = path_builder.take().transformed(&graphics_state.transform);
                    let style = graphics_state.fill_style(fill_rule(s));
                    draw(&mut scene, path, &style, graphics_state.clip_path);
                }
                "B" | "B*" => { // fill and stroke
                    path_builder.close();
                    let path = path_builder.take().transformed(&graphics_state.transform);
                    let style = graphics_state.fill_then_stroke_style(fill_rule(s));
                    draw(&mut scene, path, &style, graphics_state.clip_path);
                }
                "b" | "b*" => { // stroke and fill
                    path_builder.close();
                    let path = path_builder.take().transformed(&graphics_state.transform);
                    let style = graphics_state.stroke_then_fill_style(fill_rule(s));
                    draw(&mut scene, path, &style, graphics_state.clip_path);
                }
                "n" => { // clear path
                    path_builder.clear();
                }
                "W" | "W*" => {
                    let path = path_builder.take().transformed(&graphics_state.transform);
                    /*
                    let style = surface.build_style(PathStyle {
                        fill: Some(Paint::Solid((0, 0, 200, 50))),
                        stroke: None,
                        fill_rule: FillRule::NonZero
                    });
                    surface.draw_path(path.clone(), &style, graphics_state.clip_path.as_ref());
                    */
                    let mut clip_path = ClipPath::new(path);
                    clip_path.set_fill_rule(fill_rule(s));
                    let clip_path_id = scene.push_clip_path(clip_path);
                    graphics_state.clip_path = Some(clip_path_id);
                }
                "q" => { // save state
                    stack.push((graphics_state.clone(), text_state));
                }
                "Q" => { // restore
                    let (g, t) = stack.pop().expect("graphcs stack is empty");
                    graphics_state = g;
                    text_state = t;
                }
                "cm" => { // modify transformation matrix 
                    ops!(ops, a: f32, b: f32, c: f32, d: f32, e: f32, f: f32 => {
                        graphics_state.transform = graphics_state.transform * Transform2F::row_major(a, c, e, b, d, f);
                    })
                }
                "w" => { // line width
                    ops!(ops, width: f32 => {
                        graphics_state.stroke_width = width;
                    })
                }
                "J" => { // line cap
                }
                "j" => { // line join 
                }
                "M" => { // miter limit
                }
                "d" => { // line dash [ array phase ]
                }
                "gs" => ops!(ops, gs: &Primitive => { // set from graphic state dictionary
                    let gs = gs.as_name()?;
                    let gs = try_opt!(resources.graphics_states.get(gs));
                    if let Some(lw) = gs.line_width {
                        graphics_state.stroke_width = lw;
                    }
                    if let Some((ref font, size)) = gs.font {
                        if let Some(e) = self.get_font(&font.name) {
                            text_state.font_entry = Some(e);
                            text_state.font_size = size;
                            debug!("new font: {} at size {}", font.name, size);
                        } else {
                            text_state.font_entry = None;
                        }
                    }
                }),
                "SC" | "SCN" | "RG" => { // stroke color
                    let paint = convert_color(graphics_state.stroke_color_space, &*ops)?;
                    graphics_state.stroke_paint = scene.push_paint(&paint);
                }
                "sc" | "scn" | "rg" => { // fill color
                    let paint = convert_color(graphics_state.fill_color_space, &*ops)?;
                    graphics_state.fill_paint = scene.push_paint(&paint);
                }
                "G" => { // stroke gray
                    ops!(ops, gray: f32 => {
                        graphics_state.stroke_paint = scene.push_paint(&gray2fill(gray));
                    })
                }
                "g" => { // fill gray
                    ops!(ops, gray: f32 => {
                        graphics_state.fill_paint = scene.push_paint(&gray2fill(gray));
                    })
                }
                "K" => { // stroke color
                    ops!(ops, c: f32, m: f32, y: f32, k: f32 => {
                        graphics_state.stroke_paint = scene.push_paint(&cmyk2fill(c, m, y, k));
                    });
                }
                "k" => { // fill color
                    ops!(ops, c: f32, m: f32, y: f32, k: f32 => {
                        graphics_state.fill_paint = scene.push_paint(&cmyk2fill(c, m, y, k));
                    });
                }
                "cs" => { // color space
                    ops!(ops, name: &Primitive => {
                        let name = name.as_name()?;
                        graphics_state.fill_color_space = resources.color_spaces.get(name).unwrap().clone();
                    });
                }
                "CS" => { // color space
                    ops!(ops, name: &Primitive => {
                        let name = name.as_name()?;
                        graphics_state.stroke_color_space = resources.color_spaces.get(name).unwrap().clone();
                    });
                }
                "BT" => {
                    text_state.reset_matrix();
                }
                "ET" => {
                }
                // state modifiers
                
                // character spacing
                "Tc" => ops!(ops, char_space: f32 => {
                    text_state.char_space = char_space;
                }),
                
                // word spacing
                "Tw" => ops!(ops, word_space: f32 => {
                    text_state.word_space = word_space;
                }),
                
                // Horizontal scaling (in percent)
                "Tz" => ops!(ops, scale: f32 => {
                    text_state.horiz_scale = 0.01 * scale;
                }),
                
                // leading
                "TL" => ops!(ops, leading: f32 => {
                    text_state.leading = leading;
                }),
                
                // text font
                "Tf" => ops!(ops, font_name: &Primitive, size: f32 => {
                    let font_name = font_name.as_name()?;
                    let font = try_opt!(resources.fonts.get(font_name));
                    if let Some(e) = self.get_font(&font.name) {
                        text_state.font_entry = Some(e);
                        debug!("new font: {} (is_cid={:?})", font.name, e.is_cid);
                        text_state.font_size = size;
                    } else {
                        text_state.font_entry = None;
                    }
                }),
                
                // render mode
                "Tr" => ops!(ops, mode: i32 => {
                    use TextMode::*;
                    text_state.mode = match mode {
                        0 => Fill,
                        1 => Stroke,
                        2 => FillThenStroke,
                        3 => Invisible,
                        4 => FillAndClip,
                        5 => StrokeAndClip,
                        _ => {
                            return Err(PdfError::Other { msg: format!("Invalid text render mode: {}", mode)});
                        }
                    }
                }),
                
                // text rise
                "Ts" => ops!(ops, rise: f32 => {
                    text_state.rise = rise;
                }),
                
                // positioning operators
                // Move to the start of the next line
                "Td" => ops_p!(ops, t => {
                    text_state.translate(t);
                }),
                
                "TD" => ops_p!(ops, t => {
                    text_state.leading = -t.y();
                    text_state.translate(t);
                }),
                
                // Set the text matrix and the text line matrix
                "Tm" => ops!(ops, a: f32, b: f32, c: f32, d: f32, e: f32, f: f32 => {
                    text_state.set_matrix(Transform2F::row_major(a, c, e, b, d, f));
                }),
                
                // Move to the start of the next line
                "T*" => {
                    text_state.next_line();
                },
                
                // draw text
                "Tj" => ops!(ops, text: &[u8] => {
                    let style = graphics_state.get_text_style(text_state.mode);
                    let bb = text_state.draw_text(
                        graphics_state.transform,
                        |path| draw(&mut scene, path, &style, graphics_state.clip_path),
                        text
                    );
                    add_item(bb, op);
                }),
                
                // move to the next line and draw text
                "'" => ops!(ops, text: &[u8] => {
                    let style = graphics_state.get_text_style(text_state.mode);
                    text_state.next_line();
                    let bb = text_state.draw_text(
                        graphics_state.transform,
                        |path| draw(&mut scene, path, &style, graphics_state.clip_path),
                        text
                    );
                    add_item(bb, op);
                }),
                
                // set word and charactr spacing, move to the next line and draw text
                "\"" => ops!(ops, word_space: f32, char_space: f32, text: &[u8] => {
                    let style = graphics_state.get_text_style(text_state.mode);
                    text_state.word_space = word_space;
                    text_state.char_space = char_space;
                    text_state.next_line();
                    let bb = text_state.draw_text(
                        graphics_state.transform,
                        |path| draw(&mut scene, path, &style, graphics_state.clip_path),
                        text
                    );
                    add_item(bb, op);
                }),
                "TJ" => ops!(ops, array: &[Primitive] => {
                    let mut bb = BBox::empty();
                    let style = graphics_state.get_text_style(text_state.mode);
                    for arg in array {
                        match arg {
                            Primitive::String(ref data) => {
                                let r2 = text_state.draw_text(
                                    graphics_state.transform,
                                    |path| draw(&mut scene, path, &style, graphics_state.clip_path),
                                    data.as_bytes()
                                );
                                bb.add_bbox(r2);
                            },
                            p => {
                                let offset = p.as_number().expect("wrong argument to TJ");
                                text_state.advance(-0.001 * offset); // because why not PDF…
                            }
                        }
                    }
                    add_item(bb, op);
                }),
                "Do" => ops!(ops, name: &Primitive => {
                    (|| -> Result<()> {
                    let name = name.as_name()?;
                    let &xobject_ref = resources.xobjects.get(name).unwrap();
                    let xobject = file.get(xobject_ref)?;
                    match *xobject {
                        XObject::Image(ref image) => {
                            let raw_data = image.data()?;
                            let data = match raw_data.len() / (image.width as usize * image.height as usize) {
                                1 => raw_data.iter().map(|&l| ColorU { r: l, g: l, b: l, a: 255 }).collect(),
                                3 => raw_data.chunks(3).map(|c| ColorU { r: c[0], g: c[1], b: c[2], a: 255 }).collect(),
                                4 => raw_data.chunks(4).map(|c| ColorU{ r: c[0], g: c[1], b: c[2], a: c[3] }).collect(),
                                n => panic!("unimplemented {} bytes/pixel", n)
                            };
                            let size = Vector2I::new(image.width as _, image.height as _);
                            let size_f = size.to_f32();
                            let mut path_builder: PathBuilder = PathBuilder::new();
                            path_builder.rect(RectF::new(Vector2F::default(), Vector2F::new(1.0, 1.0)));
                            let im_tr = graphics_state.transform
                                * Transform2F::from_scale(Vector2F::new(1.0 / size_f.x(), -1.0 / size_f.y()))
                                * Transform2F::from_translation(Vector2F::new(0.0, -size_f.y()));
                            let image = Image::new(size, Arc::new(data));
                            let mut pattern = Pattern::from_image(image);
                            pattern.apply_transform(im_tr);
                            let style = PathStyle {
                                mode: DrawMode::Fill(
                                    scene.push_paint(&Paint::from_pattern(pattern))
                                ),
                                fill_rule: FillRule::Winding
                            };
                            draw(&mut scene, path_builder.take().transformed(&graphics_state.transform), &style, None);
                        },
                        _ => {}
                    }
                    Ok(())
                    })();
                }),
                _ => {}
            }
        }
        
        Ok((scene, ItemMap(items)))
    }
}
