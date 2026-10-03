//! usvg が書き出した正規化 SVG の属性を、要素ごとに直接書き換える。
//!
//! 以前は `* { fill-opacity: 1; paint-order: … }` のような CSS を注入していたが、usvg は
//! 表示属性を写したあとに CSS で上書きするので、SVG 自身の値と親からの継承を打ち消していた
//! （issues/20260922_injected_css_overrides_svg_attributes.md）。
//!
//! usvg の書き出し（`Tree::to_string`）では、CSS と継承が解決済みで、描かれるパスには
//! `fill` / `stroke` が必ず明示される（無ければ `none`）。viewBox の変換は子の `<g transform>` に、
//! クリップ・マスク・パターン・グラデーションは `<defs>` に入る。ここでは `<defs>` の外の
//! `path` / `image` だけを書き換える。

use std::collections::{HashMap, HashSet};

use aviutl2::anyhow::{self, Context};
use kurbo::{Affine, BezPath, ParamCurveArclen, PathEl};

use crate::render::Style;
use crate::{DashPreset, PaintOrder, StrokeLinecap, StrokeLinejoin, StrokeWidthUnit, TrimMode};

const SVG_NS: &str = "http://www.w3.org/2000/svg";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// 書き換えが要るか。どれも既定値なら元の SVG をそのまま描く
pub fn needs_rewrite(style: &Style) -> bool {
    style.override_fill
        || style.fill_opacity < 1.0
        || style.stroke.enabled
        || style.stroke.opacity < 1.0
        || style.paint_order == PaintOrder::StrokeThenFill
        || style.trim.is_active()
        || !style.element_id.is_empty()
}

/// カスタム線種を数値の列にする。`}` や `;` などを CSS / 属性へそのまま流さない
pub fn parse_dash(raw: &str) -> Vec<f32> {
    // 数値に使う文字以外はすべて区切りとみなす（`3;` の `;` や `fill:red` を落とす）
    let values: Vec<f32> = raw
        .split(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E')))
        .filter_map(|s| s.parse::<f32>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .collect();
    if values.iter().all(|v| *v == 0.0) {
        Vec::new()
    } else {
        values
    }
}

type Attrs = Vec<(String, String)>;

fn get<'a>(attrs: &'a Attrs, key: &str) -> Option<&'a str> {
    attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn set(attrs: &mut Attrs, key: &str, value: impl Into<String>) {
    let value = value.into();
    match attrs.iter_mut().find(|(k, _)| k == key) {
        Some(slot) => slot.1 = value,
        None => attrs.push((key.to_string(), value)),
    }
}

fn remove(attrs: &mut Attrs, keys: &[&str]) {
    attrs.retain(|(k, _)| !keys.contains(&k.as_str()));
}

fn num(v: f64) -> String {
    if v.is_finite() { format!("{v}") } else { "0".into() }
}

fn hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn opacity_of(attrs: &Attrs, key: &str) -> f64 {
    get(attrs, key).and_then(|v| v.parse().ok()).unwrap_or(1.0)
}

const STROKE_ATTRS: &[&str] = &[
    "stroke-opacity",
    "stroke-width",
    "stroke-linecap",
    "stroke-linejoin",
    "stroke-miterlimit",
    "stroke-dasharray",
    "stroke-dashoffset",
];

fn attrs_of(node: roxmltree::Node) -> Attrs {
    node.attributes()
        .map(|a| {
            let name = match a.namespace() {
                Some(XLINK_NS) => format!("xlink:{}", a.name()),
                Some(XML_NS) => format!("xml:{}", a.name()),
                _ => a.name().to_string(),
            };
            (name, a.value().to_string())
        })
        .collect()
}

fn is_defs(node: roxmltree::Node) -> bool {
    node.is_element() && node.tag_name().name() == "defs"
}

/// usvg の書き出しは `matrix(a b c d e f)` の形だけ
fn parse_matrix(value: &str) -> Option<Affine> {
    let inner = value.trim().strip_prefix("matrix(")?.strip_suffix(')')?;
    let n: Vec<f64> = inner
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    (n.len() == 6).then(|| Affine::new([n[0], n[1], n[2], n[3], n[4], n[5]]))
}

/// 描画進行で分けたサブパス 1 本
struct Piece {
    d: String,
    len: f64,
    window: Window,
}

#[derive(Clone, Copy, PartialEq)]
enum Window {
    Full,
    Hidden,
    /// 見せる区間（サブパスの始点からの長さ）
    Part(f64, f64),
}

struct PathPlan {
    attrs: Attrs,
    /// 描画進行で分けるときの pieces の範囲
    pieces: Option<std::ops::Range<usize>>,
}

struct Planner<'s> {
    style: &'s Style,
    render_scale: f64,
    target: Option<roxmltree::NodeId>,
    plans: HashMap<roxmltree::NodeId, PathPlan>,
    hidden: HashSet<roxmltree::NodeId>,
    pieces: Vec<Piece>,
}

pub fn rewrite(svg: &str, style: &Style, render_scale: f32) -> anyhow::Result<String> {
    let doc = roxmltree::Document::parse(svg).context("正規化した SVG を読めない")?;

    let target = if style.element_id.is_empty() {
        None
    } else {
        let id = style.element_id.as_str();
        let found = doc.descendants().find(|n| {
            n.is_element() && n.attribute("id") == Some(id) && !n.ancestors().any(is_defs)
        });
        Some(
            found
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "要素ID「{id}」が見つからない（g / path / use などに付いた id を指定する）"
                    )
                })?
                .id(),
        )
    };

    let mut planner = Planner {
        style,
        render_scale: f64::from(render_scale).max(1e-9),
        target,
        plans: HashMap::new(),
        hidden: HashSet::new(),
        pieces: Vec::new(),
    };
    planner.plan(doc.root_element(), Affine::IDENTITY, target.is_none());
    planner.assign_windows();

    let mut out = String::with_capacity(svg.len() + svg.len() / 4);
    planner.write(doc.root_element(), &mut out, true);
    Ok(out)
}

impl Planner<'_> {
    fn plan(&mut self, node: roxmltree::Node, ctm: Affine, in_target: bool) {
        for child in node.children().filter(|n| n.is_element()) {
            if is_defs(child) {
                continue;
            }
            let in_target = in_target || Some(child.id()) == self.target;
            let ctm = match child.attribute("transform").and_then(parse_matrix) {
                Some(local) => ctm * local,
                None => ctm,
            };
            match child.tag_name().name() {
                "path" if in_target => self.plan_path(child, ctm),
                "path" | "image" if !in_target => {
                    self.hidden.insert(child.id());
                }
                _ => {}
            }
            self.plan(child, ctm, in_target);
        }
    }

    fn plan_path(&mut self, node: roxmltree::Node, ctm: Affine) {
        let style = self.style;
        let mut attrs = attrs_of(node);

        // 塗り。fill="none" は上書きしない（継承も usvg が解決済み）
        if get(&attrs, "fill") != Some("none") {
            if style.override_fill {
                set(&mut attrs, "fill", hex(style.color));
            }
            if style.fill_opacity < 1.0 {
                let v = opacity_of(&attrs, "fill-opacity") * f64::from(style.fill_opacity);
                set(&mut attrs, "fill-opacity", num(v));
            }
        }

        // 線
        let s = &style.stroke;
        if s.enabled {
            // SVG 単位なら 1、出力 px ならこの要素のローカル座標 1 単位が何 px になるかの逆数
            let unit = match s.width_unit {
                StrokeWidthUnit::SvgUnit => 1.0,
                StrokeWidthUnit::OutputPx => {
                    1.0 / (self.render_scale * ctm.determinant().abs().sqrt()).max(1e-9)
                }
            };
            let width = f64::from(s.width) * unit;
            remove(&mut attrs, STROKE_ATTRS);
            if width > 0.0 {
                set(&mut attrs, "stroke", hex(s.color));
                if s.opacity < 1.0 {
                    set(&mut attrs, "stroke-opacity", num(f64::from(s.opacity)));
                }
                set(&mut attrs, "stroke-width", num(width));
                match s.linecap {
                    StrokeLinecap::Butt => {}
                    StrokeLinecap::Round => set(&mut attrs, "stroke-linecap", "round"),
                    StrokeLinecap::Square => set(&mut attrs, "stroke-linecap", "square"),
                }
                match s.linejoin {
                    StrokeLinejoin::Miter => {}
                    StrokeLinejoin::Round => set(&mut attrs, "stroke-linejoin", "round"),
                    StrokeLinejoin::Bevel => set(&mut attrs, "stroke-linejoin", "bevel"),
                }
                set(&mut attrs, "stroke-miterlimit", num(f64::from(s.miterlimit.max(1.0))));
                // 線種の比率は線幅に対する倍数。細い線（1 未満）では 1 を単位にする（以前と同じ）
                let dash_unit = f64::from(s.width.max(1.0)) * unit;
                let dash: Vec<f64> = match s.dash_preset {
                    DashPreset::Solid => Vec::new(),
                    DashPreset::Custom => s.custom_dash.iter().map(|v| f64::from(*v) * unit).collect(),
                    preset => preset
                        .pattern()
                        .unwrap_or(&[])
                        .iter()
                        .map(|p| f64::from(*p) * dash_unit)
                        .collect(),
                };
                if !dash.is_empty() {
                    let list: Vec<String> = dash.into_iter().map(num).collect();
                    set(&mut attrs, "stroke-dasharray", list.join(" "));
                    if s.dashoffset != 0.0 {
                        set(&mut attrs, "stroke-dashoffset", num(f64::from(s.dashoffset) * unit));
                    }
                }
            } else {
                set(&mut attrs, "stroke", "none");
            }
        } else if s.opacity < 1.0 && get(&attrs, "stroke").is_some_and(|v| v != "none") {
            let v = opacity_of(&attrs, "stroke-opacity") * f64::from(s.opacity);
            set(&mut attrs, "stroke-opacity", num(v));
        }

        if style.paint_order == PaintOrder::StrokeThenFill {
            set(&mut attrs, "paint-order", "stroke");
        }

        // 描画進行: 線のあるパスをサブパスごとに分け、長さを測っておく
        let stroked = get(&attrs, "stroke").is_some_and(|v| v != "none");
        let pieces = if style.trim.is_active() && stroked {
            let start = self.pieces.len();
            for (d, len) in split_subpaths(get(&attrs, "d").unwrap_or_default()) {
                self.pieces.push(Piece {
                    d,
                    len,
                    window: Window::Full,
                });
            }
            Some(start..self.pieces.len())
        } else {
            None
        };
        self.plans.insert(node.id(), PathPlan { attrs, pieces });
    }

    fn assign_windows(&mut self) {
        let trim = self.style.trim;
        let (a, b) = (f64::from(trim.start), f64::from(trim.end));
        let total: f64 = self.pieces.iter().map(|p| p.len).sum();
        let mut before = 0.0;
        for piece in &mut self.pieces {
            let len = piece.len;
            let (s, e) = match trim.mode {
                TrimMode::Individual => (a * len, b * len),
                TrimMode::Sequential => (
                    (a * total - before).clamp(0.0, len),
                    (b * total - before).clamp(0.0, len),
                ),
            };
            before += len;
            let eps = 1e-6 * len.max(1.0);
            piece.window = if e - s <= eps {
                Window::Hidden
            } else if s <= eps && e >= len - eps {
                Window::Full
            } else {
                Window::Part(s, e)
            };
        }
    }

    fn write(&self, node: roxmltree::Node, out: &mut String, is_root: bool) {
        if let Some(plan) = self.plans.get(&node.id()) {
            self.write_path(plan, out);
            return;
        }
        let mut attrs = attrs_of(node);
        if self.hidden.contains(&node.id()) {
            set(&mut attrs, "visibility", "hidden");
        }
        let name = node.tag_name().name();
        out.push('<');
        out.push_str(name);
        if is_root {
            out.push_str(" xmlns=\"");
            out.push_str(SVG_NS);
            out.push_str("\" xmlns:xlink=\"");
            out.push_str(XLINK_NS);
            out.push('"');
        }
        write_attrs(&attrs, out);
        let children: Vec<_> = node.children().collect();
        if children.is_empty() {
            out.push_str("/>");
            return;
        }
        out.push('>');
        for child in children {
            if child.is_element() {
                self.write(child, out, false);
            } else if let Some(text) = child.text().filter(|_| child.is_text()) {
                escape(text, false, out);
            }
        }
        out.push_str("</");
        out.push_str(name);
        out.push('>');
    }

    fn write_path(&self, plan: &PathPlan, out: &mut String) {
        let Some(range) = plan.pieces.clone() else {
            write_empty("path", &plan.attrs, out);
            return;
        };
        let pieces = &self.pieces[range];
        if pieces.iter().all(|p| p.window == Window::Full) {
            write_empty("path", &plan.attrs, out);
            return;
        }

        // 塗りは元の形のまま 1 本（穴のあるパスを分けると塗りが変わる）、線はサブパスごと
        let stroke_first = get(&plan.attrs, "paint-order").is_some_and(|v| v.starts_with("stroke"));
        let fill = (get(&plan.attrs, "fill") != Some("none")).then(|| {
            let mut a = plan.attrs.clone();
            remove(&mut a, STROKE_ATTRS);
            remove(&mut a, &["paint-order"]);
            set(&mut a, "stroke", "none");
            a
        });
        let strokes: Vec<Attrs> = pieces
            .iter()
            .filter(|p| p.window != Window::Hidden)
            .map(|p| {
                let mut a = plan.attrs.clone();
                remove(&mut a, &["id", "paint-order", "fill-opacity", "fill-rule"]);
                set(&mut a, "fill", "none");
                set(&mut a, "d", p.d.clone());
                if let Window::Part(s, e) = p.window {
                    // 長さ e-s の線を 1 本だけ見せる。間隔をサブパス長にすれば 2 本目は現れない
                    let dash = e - s;
                    set(&mut a, "stroke-dasharray", format!("{} {}", num(dash), num(p.len)));
                    set(&mut a, "stroke-dashoffset", num(dash + p.len - s));
                }
                a
            })
            .collect();

        if !stroke_first {
            if let Some(a) = &fill {
                write_empty("path", a, out);
            }
        }
        for a in &strokes {
            write_empty("path", a, out);
        }
        if stroke_first {
            if let Some(a) = &fill {
                write_empty("path", a, out);
            }
        }
    }
}

/// パスをサブパスごとに分け、それぞれの長さを返す（長さ 0 のものは捨てる）
fn split_subpaths(d: &str) -> Vec<(String, f64)> {
    let Ok(path) = BezPath::from_svg(d) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut current = BezPath::new();
    let flush = |p: &BezPath, out: &mut Vec<(String, f64)>| {
        let len: f64 = p.segments().map(|s| s.arclen(1e-3)).sum();
        if len > 1e-9 {
            out.push((p.to_svg(), len));
        }
    };
    for el in path.elements() {
        if matches!(el, PathEl::MoveTo(_)) && !current.elements().is_empty() {
            flush(&current, &mut out);
            current = BezPath::new();
        }
        current.push(*el);
    }
    flush(&current, &mut out);
    out
}

fn write_attrs(attrs: &Attrs, out: &mut String) {
    for (k, v) in attrs {
        out.push(' ');
        out.push_str(k);
        out.push_str("=\"");
        escape(v, true, out);
        out.push('"');
    }
}

fn write_empty(name: &str, attrs: &Attrs, out: &mut String) {
    out.push('<');
    out.push_str(name);
    write_attrs(attrs, out);
    out.push_str("/>");
}

fn escape(s: &str, attr: bool, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::tests::params;

    #[test]
    fn defaults_need_no_rewrite() {
        assert!(!needs_rewrite(&params().style));
    }

    #[test]
    fn custom_dash_is_sanitized() {
        assert_eq!(parse_dash("5,3; fill:red }"), vec![5.0, 3.0]);
        assert_eq!(parse_dash("4 2 1"), vec![4.0, 2.0, 1.0]);
        assert!(parse_dash("").is_empty());
        assert!(parse_dash("0 0").is_empty());
        assert!(parse_dash("-1 abc").is_empty());
    }

    #[test]
    fn subpaths_are_measured() {
        let parts = split_subpaths("M 0 0 L 10 0 M 0 5 L 0 25 Z M 3 3");
        assert_eq!(parts.len(), 2);
        assert!((parts[0].1 - 10.0).abs() < 1e-6);
        assert!((parts[1].1 - 40.0).abs() < 1e-6);
    }

    #[test]
    fn rewrite_keeps_xlink_and_text() {
        let svg = r##"<svg width="10" height="10" xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><defs><pattern id="p"><path d="M 0 0 L 1 1" fill="#000000" stroke="none"/></pattern></defs><image xlink:href="data:x" width="1" height="1"/><path d="M 0 0 L 5 5" fill="url(#p)" stroke="none"/></svg>"##;
        let mut p = params();
        p.style.override_fill = true;
        let out = rewrite(svg, &p.style, 1.0).unwrap();
        assert!(out.contains(r#"xlink:href="data:x""#), "{out}");
        // defs の中は書き換えない
        assert!(out.contains(r##"<path d="M 0 0 L 1 1" fill="#000000""##), "{out}");
        assert!(out.contains(r##"fill="#ffffff""##), "{out}");
    }
}
