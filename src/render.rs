//! SVG を画素にする処理（ホストに依存しない部分。単体テストはここで回す）
//!
//! 流れ:
//! 1. `currentColor` だけを CSS で決めて usvg で読む（本家 svg.aux2 と同じ）
//! 2. 上書き系の設定があるときだけ、正規化 SVG へ書き出して属性を書き換え、読み直す（`rewrite`）
//! 3. viewBox（クリッピング後）を出力サイズへ写し、線の上書きのはみ出し分だけ 4 辺に余白を取る
//! 4. 描画して、乗算済みの画素をストレートへ戻す（`PIXEL_RGBA` はストレート）

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aviutl2::anyhow::{self, Context};
use resvg::{tiny_skia, usvg};

use crate::rewrite;
use crate::{
    DashPreset, PaintOrder, SizeBasis, StrokeLinecap, StrokeLinejoin, StrokeWidthUnit, TrimMode,
};

/// 出力の 1 辺の上限。超えるときは切り詰めずに倍率を下げる
pub const MAX_CANVAS: u32 = 8192;

#[derive(Clone)]
pub enum Source {
    File(PathBuf),
    Inline(String),
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::File(path) => write!(f, "'{}'", path.display()),
            Source::Inline(data) => write!(f, "インライン入力（{} バイト）", data.len()),
        }
    }
}

/// SVG の中身と、相対パスの外部画像を解決する基準フォルダを返す
pub fn load(source: &Source) -> anyhow::Result<(Vec<u8>, Option<PathBuf>)> {
    match source {
        Source::File(path) => {
            let data = std::fs::read(path)
                .with_context(|| format!("SVG ファイルを読めない: '{}'", path.display()))?;
            Ok((data, path.parent().map(Path::to_path_buf)))
        }
        Source::Inline(text) => Ok((text.as_bytes().to_vec(), None)),
    }
}

#[derive(Debug, Clone)]
pub struct StrokeStyle {
    pub enabled: bool,
    pub color: (u8, u8, u8),
    pub width: f32,
    pub width_unit: StrokeWidthUnit,
    /// 0..=1
    pub opacity: f32,
    pub dash_preset: DashPreset,
    /// 数値だけに絞ったカスタム線種（`rewrite::parse_dash`）
    pub custom_dash: Vec<f32>,
    pub dashoffset: f32,
    pub linecap: StrokeLinecap,
    pub linejoin: StrokeLinejoin,
    pub miterlimit: f32,
}

/// 線の描画進行。start / end は 0..=1
#[derive(Debug, Clone, Copy)]
pub struct Trim {
    pub start: f32,
    pub end: f32,
    pub mode: TrimMode,
}

impl Trim {
    pub fn is_active(&self) -> bool {
        self.start > 1e-4 || self.end < 1.0 - 1e-4
    }
}

#[derive(Debug, Clone)]
pub struct Style {
    /// `currentColor`（既存の「色」）
    pub color: (u8, u8, u8),
    pub override_fill: bool,
    /// 0..=1。SVG 自身の fill-opacity に掛ける
    pub fill_opacity: f32,
    pub paint_order: PaintOrder,
    pub stroke: StrokeStyle,
    pub trim: Trim,
    /// 空でなければ、この id の要素だけを元の位置のまま描く
    pub element_id: String,
}

#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub keep_aspect: bool,
    pub basis: SizeBasis,
    /// 左・上・右・下（SVG の width / height の単位）
    pub clip: [f32; 4],
}

#[derive(Debug, Clone)]
pub struct RenderParams {
    pub style: Style,
    pub layout: Layout,
}

impl Hash for RenderParams {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let s = &self.style;
        s.color.hash(state);
        s.override_fill.hash(state);
        s.fill_opacity.to_bits().hash(state);
        s.paint_order.hash(state);
        let k = &s.stroke;
        k.enabled.hash(state);
        k.color.hash(state);
        k.width.to_bits().hash(state);
        k.width_unit.hash(state);
        k.opacity.to_bits().hash(state);
        k.dash_preset.hash(state);
        for v in &k.custom_dash {
            v.to_bits().hash(state);
        }
        k.dashoffset.to_bits().hash(state);
        k.linecap.hash(state);
        k.linejoin.hash(state);
        k.miterlimit.to_bits().hash(state);
        s.trim.start.to_bits().hash(state);
        s.trim.end.to_bits().hash(state);
        s.trim.mode.hash(state);
        s.element_id.hash(state);
        let l = &self.layout;
        l.width.hash(state);
        l.height.hash(state);
        l.keep_aspect.hash(state);
        l.basis.hash(state);
        for v in l.clip {
            v.to_bits().hash(state);
        }
    }
}

/// ストレート（非乗算）アルファの RGBA
pub struct Rendered {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub fn render(
    data: &[u8],
    resources_dir: Option<&Path>,
    fontdb: &Arc<usvg::fontdb::Database>,
    params: &RenderParams,
) -> anyhow::Result<Rendered> {
    let (r, g, b) = params.style.color;
    let opt = usvg::Options {
        // 本家と同じく currentColor だけを決める。ほかの上書きは rewrite で属性を書き換える。
        // `* { fill-opacity: 1 }` のような CSS は SVG 自身の表示属性を打ち消すので使わない
        style_sheet: Some(format!("* {{ color: rgb({r},{g},{b}) }}")),
        fontdb: Arc::clone(fontdb),
        resources_dir: resources_dir.map(Path::to_path_buf),
        ..Default::default()
    };
    let mut tree = usvg::Tree::from_data(data, &opt).context("SVG を解釈できない")?;

    let view = ViewRect::new(tree.size(), params.layout.clip);
    let (sx, sy) = base_scale(&view, &params.layout);

    if rewrite::needs_rewrite(&params.style) {
        let normalized = tree.to_string(&usvg::WriteOptions::default());
        let rewritten = rewrite::rewrite(&normalized, &params.style, (sx * sy).sqrt())?;
        let opt = usvg::Options {
            fontdb: Arc::clone(fontdb),
            ..Default::default()
        };
        tree = usvg::Tree::from_str(&rewritten, &opt).context("書き換えた SVG を解釈できない")?;
    }

    // クリッピングしているときは切り口を優先して余白を取らない
    let clipped = params.layout.clip.iter().any(|c| *c > 0.0);
    let pad = if params.style.stroke.enabled && !clipped {
        stroke_pad(&tree, &view)
    } else {
        0.0
    };
    let geometry = Geometry::new(&view, sx, sy, pad);

    let mut pixmap = tiny_skia::Pixmap::new(geometry.width, geometry.height).ok_or_else(|| {
        anyhow::anyhow!("{}x{} の画像を確保できない", geometry.width, geometry.height)
    })?;
    resvg::render(&tree, geometry.transform, &mut pixmap.as_mut());
    let mut rgba = pixmap.take();
    unpremultiply_rgba(&mut rgba);
    Ok(Rendered {
        width: geometry.width,
        height: geometry.height,
        rgba,
    })
}

/// 描く範囲（クリッピング後の viewBox）。SVG の width / height の単位
struct ViewRect {
    x0: f32,
    y0: f32,
    w: f32,
    h: f32,
}

impl ViewRect {
    fn new(size: usvg::Size, clip: [f32; 4]) -> Self {
        const MIN_SIZE: f32 = 0.01;
        let [left, top, right, bottom] = clip.map(|c| c.max(0.0));
        let x0 = left;
        let y0 = top;
        let x1 = (size.width() - right).max(x0 + MIN_SIZE);
        let y1 = (size.height() - bottom).max(y0 + MIN_SIZE);
        Self {
            x0,
            y0,
            w: x1 - x0,
            h: y1 - y0,
        }
    }

    fn x1(&self) -> f32 {
        self.x0 + self.w
    }

    fn y1(&self) -> f32 {
        self.y0 + self.h
    }
}

fn base_scale(view: &ViewRect, layout: &Layout) -> (f32, f32) {
    let sx = layout.width.max(1) as f32 / view.w;
    let sy = layout.height.max(1) as f32 / view.h;
    if !layout.keep_aspect {
        return (sx, sy);
    }
    let s = match layout.basis {
        SizeBasis::Width => sx,
        SizeBasis::Height => sy,
        SizeBasis::Fit => sx.min(sy),
    };
    (s, s)
}

/// 線の上書きで viewBox の端からはみ出す線を収める余白（SVG の単位、4 辺共通）。
///
/// 線そのものの張り出し（線込みの外接矩形 − 図形の外接矩形）を上限にするので、
/// viewBox の外に置かれた図形までは取り込まない。4 辺同じ幅にして画像の中心を viewBox の中心に保つ
fn stroke_pad(tree: &usvg::Tree, view: &ViewRect) -> f32 {
    let fill_bb = tree.root().abs_bounding_box();
    let stroke_bb = tree.root().abs_stroke_bounding_box();
    let extent = [
        fill_bb.left() - stroke_bb.left(),
        stroke_bb.right() - fill_bb.right(),
        fill_bb.top() - stroke_bb.top(),
        stroke_bb.bottom() - fill_bb.bottom(),
    ]
    .into_iter()
    .fold(0.0f32, f32::max);
    let overflow = [
        view.x0 - stroke_bb.left(),
        stroke_bb.right() - view.x1(),
        view.y0 - stroke_bb.top(),
        stroke_bb.bottom() - view.y1(),
    ]
    .into_iter()
    .fold(0.0f32, f32::max);
    overflow.min(extent).max(0.0)
}

struct Geometry {
    width: u32,
    height: u32,
    transform: tiny_skia::Transform,
}

impl Geometry {
    fn new(view: &ViewRect, sx: f32, sy: f32, pad: f32) -> Self {
        let exact = |sx: f32, sy: f32| {
            (
                (view.w + 2.0 * pad) * sx,
                (view.h + 2.0 * pad) * sy,
            )
        };
        let (mut sx, mut sy) = (sx, sy);
        let (mut ew, mut eh) = exact(sx, sy);
        let max = MAX_CANVAS as f32;
        if ew > max || eh > max {
            // 切り詰めると右端・下端が欠けて中心もずれるので、縦横同じ比で縮める
            let k = (max / ew).min(max / eh);
            sx *= k;
            sy *= k;
            (ew, eh) = exact(sx, sy);
        }
        // 0.6 * 100 = 60.000002 のような誤差で 1px 増やさない
        let ceil_px = |v: f32| ((v - 1e-3).ceil().max(1.0) as u32).min(MAX_CANVAS);
        let width = ceil_px(ew);
        let height = ceil_px(eh);
        // ceil で増えた端数は両側へ半分ずつ振り、中心を保つ
        let tx = pad * sx + (width as f32 - ew) * 0.5;
        let ty = pad * sy + (height as f32 - eh) * 0.5;
        let transform = tiny_skia::Transform::from_scale(sx, sy)
            .pre_translate(-view.x0, -view.y0)
            .post_translate(tx, ty);
        Self {
            width,
            height,
            transform,
        }
    }
}

/// tiny-skia の乗算済み RGBA をストレートに戻す（本家 svg.aux2 v0.5.1 と同じ）
pub fn unpremultiply_rgba(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        if alpha == 0 {
            pixel[..3].fill(0);
            continue;
        }
        for channel in &mut pixel[..3] {
            let straight = (u16::from(*channel) * 255 + alpha / 2) / alpha;
            *channel = straight.min(255) as u8;
        }
    }
}

/// 読めなかったときに代わりに出す絵（赤い枠と ×）。欠けていることに気づけるようにする
pub fn placeholder(layout: &Layout) -> Rendered {
    let (w, h) = {
        let (w, h) = (layout.width.max(1), layout.height.max(1));
        let (w, h) = if layout.keep_aspect {
            let s = match layout.basis {
                SizeBasis::Width => w,
                SizeBasis::Height => h,
                SizeBasis::Fit => w.min(h),
            };
            (s, s)
        } else {
            (w, h)
        };
        (w.min(MAX_CANVAS), h.min(MAX_CANVAS))
    };
    let Some(mut pixmap) = tiny_skia::Pixmap::new(w, h) else {
        return Rendered {
            width: 1,
            height: 1,
            rgba: vec![0; 4],
        };
    };
    let t = (w.min(h) as f32 / 40.0).max(2.0);
    let (x0, y0, x1, y1) = (t * 0.5, t * 0.5, w as f32 - t * 0.5, h as f32 - t * 0.5);
    let mut pb = tiny_skia::PathBuilder::new();
    pb.move_to(x0, y0);
    pb.line_to(x1, y0);
    pb.line_to(x1, y1);
    pb.line_to(x0, y1);
    pb.close();
    pb.move_to(x0, y0);
    pb.line_to(x1, y1);
    pb.move_to(x1, y0);
    pb.line_to(x0, y1);
    if let Some(path) = pb.finish() {
        let mut paint = tiny_skia::Paint::default();
        paint.set_color_rgba8(230, 60, 60, 220);
        paint.anti_alias = true;
        let stroke = tiny_skia::Stroke {
            width: t,
            ..Default::default()
        };
        pixmap.stroke_path(&path, &paint, &stroke, tiny_skia::Transform::identity(), None);
    }
    let mut rgba = pixmap.take();
    unpremultiply_rgba(&mut rgba);
    Rendered {
        width: w,
        height: h,
        rgba,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn params() -> RenderParams {
        RenderParams {
            style: Style {
                color: (255, 255, 255),
                override_fill: false,
                fill_opacity: 1.0,
                paint_order: PaintOrder::FillThenStroke,
                stroke: StrokeStyle {
                    enabled: false,
                    color: (0, 0, 0),
                    width: 1.0,
                    width_unit: StrokeWidthUnit::SvgUnit,
                    opacity: 1.0,
                    dash_preset: DashPreset::Solid,
                    custom_dash: Vec::new(),
                    dashoffset: 0.0,
                    linecap: StrokeLinecap::Butt,
                    linejoin: StrokeLinejoin::Miter,
                    miterlimit: 4.0,
                },
                trim: Trim {
                    start: 0.0,
                    end: 1.0,
                    mode: TrimMode::Individual,
                },
                element_id: String::new(),
            },
            layout: Layout {
                width: 100,
                height: 100,
                keep_aspect: true,
                basis: SizeBasis::Width,
                clip: [0.0; 4],
            },
        }
    }

    fn fontdb() -> Arc<usvg::fontdb::Database> {
        Arc::new(usvg::fontdb::Database::new())
    }

    pub fn draw(svg: &str, p: &RenderParams) -> Rendered {
        render(svg.as_bytes(), None, &fontdb(), p).expect("render")
    }

    impl Rendered {
        pub fn px(&self, x: u32, y: u32) -> [u8; 4] {
            let i = ((y * self.width + x) * 4) as usize;
            [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2], self.rgba[i + 3]]
        }

        pub fn count(&self, rgb: (u8, u8, u8)) -> usize {
            self.rgba
                .chunks_exact(4)
                .filter(|p| p[3] == 255 && (p[0], p[1], p[2]) == rgb)
                .count()
        }
    }

    const RED: (u8, u8, u8) = (255, 0, 0);
    const BLUE: (u8, u8, u8) = (0, 0, 255);

    const HALVES: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
        <rect x="0" y="0" width="50" height="100" fill="#ff0000"/>
        <rect x="50" y="0" width="50" height="100" fill="#0000ff"/></svg>"##;

    #[test]
    fn default_keeps_svg_fill_opacity() {
        // 注入 CSS が表示属性を打ち消していた不具合（issues/20260922_injected_css_overrides_svg_attributes.md）
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
            <rect x="0" y="0" width="50" height="100" fill="#ff0000" fill-opacity="0.5"/>
            <g fill-opacity="0.5"><rect x="50" y="0" width="50" height="100" fill="#0000ff"/></g></svg>"##;
        let img = draw(svg, &params());
        assert_eq!(img.px(25, 50)[3], 128);
        assert_eq!(img.px(75, 50)[3], 128);
    }

    #[test]
    fn default_keeps_svg_paint_order() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
            <rect x="20" y="20" width="60" height="60" fill="#ff0000" stroke="#0000ff" stroke-width="20" paint-order="stroke"/></svg>"##;
        let img = draw(svg, &params());
        assert_eq!(img.px(25, 50), [255, 0, 0, 255]);
    }

    #[test]
    fn output_is_straight_alpha() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
            <circle cx="50" cy="50" r="30.5" fill="#ffffff"/></svg>"##;
        let img = draw(svg, &params());
        let edges: Vec<_> = img.rgba.chunks_exact(4).filter(|p| p[3] > 20 && p[3] < 235).collect();
        assert!(!edges.is_empty());
        for p in edges {
            assert!(p[0] >= 253 && p[1] >= 253 && p[2] >= 253, "縁が暗い: {p:?}");
        }
    }

    #[test]
    fn clipping_crops() {
        // 以前は 200x200 に拡大して、切ったはずの赤も描いていた
        let mut p = params();
        p.layout.clip = [50.0, 0.0, 0.0, 0.0];
        let img = draw(HALVES, &p);
        assert_eq!((img.width, img.height), (100, 200));
        assert_eq!(img.count(RED), 0);
        assert_eq!(img.count(BLUE), 100 * 200);
    }

    #[test]
    fn clipping_accepts_fractions() {
        // 左を 50 / 49.5 / 49 切る。整数に丸められていれば 49.5 は 49 か 50 と同じになる
        let reds = |left: f32| {
            let mut p = params();
            p.layout.clip = [left, 0.0, 0.0, 0.0];
            let img = draw(HALVES, &p);
            img.rgba
                .chunks_exact(4)
                .filter(|px| px[3] > 0 && px[0] > 127 && px[2] < 128)
                .count()
        };
        let (r50, r495, r49) = (reds(50.0), reds(49.5), reds(49.0));
        assert_eq!(r50, 0);
        assert!(r495 > 0 && r495 < r49, "{r50} {r495} {r49}");
    }

    #[test]
    fn content_outside_viewbox_is_not_drawn() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100" viewBox="0 0 100 100">
            <rect x="0" y="0" width="100" height="100" fill="#0000ff"/>
            <rect x="150" y="40" width="50" height="20" fill="#ff0000"/></svg>"##;
        let img = draw(svg, &params());
        assert_eq!((img.width, img.height), (100, 100));
        assert_eq!(img.count(RED), 0);
    }

    #[test]
    fn oversize_is_scaled_down_not_cut() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="100">
            <rect width="10" height="90" fill="#0000ff"/><rect y="90" width="10" height="10" fill="#ff0000"/></svg>"##;
        let mut p = params();
        p.layout.width = 8192;
        let img = draw(svg, &p);
        assert!(img.height <= MAX_CANVAS && img.width <= MAX_CANVAS);
        assert!((img.width as i32 - 819).abs() <= 1, "{}", img.width);
        // 下端（赤）が欠けずに残る
        assert_eq!(img.px(img.width / 2, img.height - 2), [255, 0, 0, 255]);
    }

    #[test]
    fn size_basis() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50"><rect width="100" height="50"/></svg>"##;
        let mut p = params();
        p.layout.width = 200;
        p.layout.height = 40;
        let size = |p: &RenderParams| {
            let img = draw(svg, p);
            (img.width, img.height)
        };
        assert_eq!(size(&p), (200, 100));
        p.layout.basis = SizeBasis::Height;
        assert_eq!(size(&p), (80, 40));
        p.layout.basis = SizeBasis::Fit;
        assert_eq!(size(&p), (80, 40));
        p.layout.width = 60;
        assert_eq!(size(&p), (60, 30));
        p.layout.keep_aspect = false;
        assert_eq!(size(&p), (60, 40));
    }

    #[test]
    fn fill_override_keeps_inherited_none() {
        // 線だけのアイコン（ルートに fill="none"）が塗りつぶされていた
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100" fill="none" stroke="#000000" stroke-width="10">
            <rect x="20" y="20" width="60" height="60"/></svg>"##;
        let mut p = params();
        p.style.override_fill = true;
        p.style.color = (255, 0, 0);
        let img = draw(svg, &p);
        assert_eq!(img.px(50, 50)[3], 0);
        assert_eq!(img.px(20, 50), [0, 0, 0, 255]);

        let filled = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100"><rect width="100" height="100" fill="#0000ff"/></svg>"##;
        assert_eq!(draw(filled, &p).px(50, 50), [255, 0, 0, 255]);
    }

    #[test]
    fn fill_opacity_multiplies() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
            <rect width="100" height="100" fill="#0000ff" fill-opacity="0.5"/></svg>"##;
        let mut p = params();
        p.style.fill_opacity = 0.5;
        let a = draw(svg, &p).px(50, 50)[3];
        assert!((63..=65).contains(&a), "{a}");
    }

    #[test]
    fn paint_order_forced_when_selected() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
            <rect x="20" y="20" width="60" height="60" fill="#ff0000" stroke="#0000ff" stroke-width="20"/></svg>"##;
        assert_eq!(draw(svg, &params()).px(25, 50), [0, 0, 255, 255]);
        let mut p = params();
        p.style.paint_order = PaintOrder::StrokeThenFill;
        assert_eq!(draw(svg, &p).px(25, 50), [255, 0, 0, 255]);
    }

    #[test]
    fn stroke_override_pads_symmetrically() {
        // 左端に接した図形の線が外へはみ出す。余白は 4 辺同じ幅で、中心がずれない
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
            <rect x="0" y="0" width="50" height="100" fill="#0000ff"/></svg>"##;
        let mut p = params();
        p.style.stroke.enabled = true;
        p.style.stroke.width = 10.0;
        p.style.stroke.linejoin = StrokeLinejoin::Round;
        let img = draw(svg, &p);
        assert_eq!((img.width, img.height), (110, 110));
        assert_eq!(img.px(2, 55), [0, 0, 0, 255]);
        assert_eq!(img.px(107, 55)[3], 0);
    }

    #[test]
    fn svgz_is_read() {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(HALVES.as_bytes()).unwrap();
        let data = gz.finish().unwrap();
        let img = render(&data, None, &fontdb(), &params()).unwrap();
        assert_eq!(img.count(BLUE), 5000);
    }

    #[test]
    fn linked_image_is_resolved_from_resources_dir() {
        let dir = std::env::temp_dir().join(format!("svg_h_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("sub.svg"),
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="#ff0000"/></svg>"##,
        )
        .unwrap();
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="100" height="100">
            <image xlink:href="sub.svg" width="100" height="100"/></svg>"##;
        let with_dir = render(svg.as_bytes(), Some(&dir), &fontdb(), &params()).unwrap();
        let without = render(svg.as_bytes(), None, &fontdb(), &params()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(with_dir.px(50, 50), [255, 0, 0, 255]);
        assert_eq!(without.px(50, 50)[3], 0);
    }

    #[test]
    fn element_id_draws_part_in_place() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
            <g id="left"><rect width="50" height="100" fill="#ff0000"/></g>
            <g id="right"><rect x="50" width="50" height="100" fill="#0000ff"/></g></svg>"##;
        let mut p = params();
        p.style.element_id = "right".into();
        let img = draw(svg, &p);
        assert_eq!((img.width, img.height), (100, 100));
        assert_eq!(img.count(RED), 0);
        assert_eq!(img.count(BLUE), 5000);

        p.style.element_id = "missing".into();
        assert!(render(svg.as_bytes(), None, &fontdb(), &p).is_err());
    }

    const TWO_LINES: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="40">
        <path d="M 0 10 H 100" stroke="#000000" stroke-width="4" fill="none"/>
        <path d="M 0 30 H 100" stroke="#000000" stroke-width="4" fill="none"/></svg>"##;

    #[test]
    fn trim_individual() {
        let mut p = params();
        p.style.trim.end = 0.5;
        let img = draw(TWO_LINES, &p);
        assert_eq!(img.px(25, 10)[3], 255);
        assert_eq!(img.px(75, 10)[3], 0);
        assert_eq!(img.px(25, 30)[3], 255);
        assert_eq!(img.px(75, 30)[3], 0);

        p.style.trim.start = 0.5;
        p.style.trim.end = 1.0;
        let img = draw(TWO_LINES, &p);
        assert_eq!(img.px(25, 10)[3], 0);
        assert_eq!(img.px(75, 10)[3], 255);
    }

    #[test]
    fn trim_sequential() {
        let mut p = params();
        p.style.trim.mode = TrimMode::Sequential;
        p.style.trim.end = 0.5;
        let img = draw(TWO_LINES, &p);
        assert_eq!(img.px(75, 10)[3], 255);
        assert_eq!(img.px(25, 30)[3], 0);
        p.style.trim.end = 0.75;
        let img = draw(TWO_LINES, &p);
        assert_eq!(img.px(25, 30)[3], 255);
        assert_eq!(img.px(75, 30)[3], 0);
    }

    #[test]
    fn trim_keeps_fill_and_hides_stroke_at_zero() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
            <rect x="20" y="20" width="60" height="60" fill="#0000ff" stroke="#000000" stroke-width="10"/></svg>"##;
        let mut p = params();
        p.style.trim.end = 0.0;
        let img = draw(svg, &p);
        assert_eq!(img.px(50, 50), [0, 0, 255, 255]);
        assert_eq!(img.px(17, 50)[3], 0);
    }

    #[test]
    fn stroke_width_in_output_px() {
        // viewBox 10 を 100px で描く（10 倍）。出力 px で 4 を指定すると線の太さが 4px になる
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10" viewBox="0 0 10 10">
            <path d="M 0 5 H 10" fill="none" stroke="#000000"/></svg>"##;
        let mut p = params();
        p.style.stroke.enabled = true;
        p.style.stroke.width = 4.0;
        p.style.stroke.width_unit = StrokeWidthUnit::OutputPx;
        p.layout.clip = [0.0, 0.0, 0.0, 0.01]; // 余白を取らせずに太さだけ測る
        let img = draw(svg, &p);
        let thick = (0..img.height).filter(|y| img.px(50, *y)[3] > 127).count();
        assert!((3..=5).contains(&thick), "{thick}");
    }

    #[test]
    fn placeholder_has_marks() {
        let img = placeholder(&params().layout);
        assert_eq!((img.width, img.height), (100, 100));
        assert!(img.rgba.chunks_exact(4).any(|p| p[3] > 0));
    }
}
