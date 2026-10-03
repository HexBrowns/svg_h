use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use aviutl2::{
    anyhow::{self, Context},
    filter::{FilterConfigItemSliceExt, FilterConfigItems, FilterConfigSelectItems},
    tracing,
};

mod render;
mod rewrite;

use render::{Layout, RenderParams, Source, StrokeStyle, Style, Trim};

#[aviutl2::plugin(GenericPlugin)]
struct SvgHAux2 {
    filter: aviutl2::generic::SubPlugin<SvgFilter>,
}

static EDIT_HANDLE: aviutl2::generic::GlobalEditHandle = aviutl2::generic::GlobalEditHandle::new();

/// 本家 svg.aux2 と衝突しないキャッシュ名前空間。描画結果が変わる修正を入れたら版を上げる
/// （v2: ストレートアルファ・クリッピング・属性の書き換え）
const CACHE_NS: &str = "svg_h:v2:";

impl aviutl2::generic::GenericPlugin for SvgHAux2 {
    fn new(info: aviutl2::AviUtl2Info) -> aviutl2::AnyResult<Self> {
        Ok(Self {
            filter: aviutl2::generic::SubPlugin::new_filter_plugin(&info)?,
        })
    }

    fn plugin_info(&self) -> aviutl2::generic::GenericPluginTable {
        aviutl2::generic::GenericPluginTable {
            name: "svg_h.aux2".to_string(),
            information: format!(
                "Render SVG files as filter objects (stroke-extended fork) / v{} / forked from sevenc-nanashi/svg.aux2",
                env!("CARGO_PKG_VERSION")
            ),
        }
    }

    fn register(&mut self, registry: &mut aviutl2::generic::HostAppHandle) {
        registry.register_filter_plugin(&self.filter);
        let filters = aviutl2::file_filters! {
            "SVG" => ["svg", "svgz"]
        };
        EDIT_HANDLE.init(registry.create_edit_handle());
        registry.register_file_drop_handler("svg_h.aux2", &filters, |file| {
            let res = EDIT_HANDLE
                .call_edit_section(|e| {
                    let position = e.get_mouse_layer_frame()?.unwrap_or({
                        aviutl2::generic::LayerFrameData {
                            layer: e.info.layer,
                            frame: e.info.frame,
                        }
                    });

                    let mut object_alias = aviutl2::alias::Table::new();
                    let mut object = aviutl2::alias::Table::new();
                    object.insert_value(
                        "name",
                        file.file_name().and_then(|n| n.to_str()).unwrap_or("SVG_H"),
                    );
                    let mut object_0 = aviutl2::alias::Table::new();
                    object_0.insert_value("effect.name", "SVG_H");
                    object_0.insert_value(
                        "ファイル",
                        file.to_str()
                            .context("Failed to convert file path to string for object alias")?,
                    );
                    object.insert_table("0", object_0);
                    object_alias.insert_table("Object", object);

                    e.create_object_from_alias(
                        &object_alias.to_string(),
                        position.layer,
                        position.frame,
                        0,
                    )?;

                    anyhow::Ok(())
                })
                .map_err(anyhow::Error::from)
                .flatten();
            if let Err(e) = res {
                tracing::error!("Failed to handle file drop: {}", e);
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FilterConfigSelectItems)]
pub(crate) enum StrokeLinecap {
    #[item(name = "butt")]
    Butt,
    #[item(name = "round")]
    Round,
    #[item(name = "square")]
    Square,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FilterConfigSelectItems)]
pub(crate) enum StrokeLinejoin {
    #[item(name = "miter")]
    Miter,
    #[item(name = "round")]
    Round,
    #[item(name = "bevel")]
    Bevel,
}

/// Inkscape builtin dash presets (dash_0 / dash_1_1 / …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FilterConfigSelectItems)]
pub(crate) enum DashPreset {
    #[item(name = "実線")]
    Solid,
    #[item(name = "1:1")]
    Ratio1_1,
    #[item(name = "2:1")]
    Ratio2_1,
    #[item(name = "4:1")]
    Ratio4_1,
    #[item(name = "1:2")]
    Ratio1_2,
    #[item(name = "1:4")]
    Ratio1_4,
    #[item(name = "カスタム")]
    Custom,
}

impl DashPreset {
    /// Returns pattern in multiples of stroke-width, or None for solid.
    pub(crate) fn pattern(self) -> Option<&'static [f32]> {
        match self {
            Self::Solid => None,
            Self::Ratio1_1 => Some(&[1.0, 1.0]),
            Self::Ratio2_1 => Some(&[2.0, 1.0]),
            Self::Ratio4_1 => Some(&[4.0, 1.0]),
            Self::Ratio1_2 => Some(&[1.0, 2.0]),
            Self::Ratio1_4 => Some(&[1.0, 4.0]),
            Self::Custom => None,
        }
    }
}

/// SVG `paint-order`。「塗りの上に線」は SVG のまま描く（SVG の既定がこの順）。
/// 「線の上に塗り」を選んだときだけ全パスへ強制する
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FilterConfigSelectItems)]
pub(crate) enum PaintOrder {
    #[item(name = "塗りの上に線")]
    FillThenStroke,
    #[item(name = "線の上に塗り")]
    StrokeThenFill,
}

/// 「アスペクト比の維持」のときに大きさを決める基準
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FilterConfigSelectItems)]
pub(crate) enum SizeBasis {
    #[item(name = "幅")]
    Width,
    #[item(name = "高さ")]
    Height,
    #[item(name = "枠に収める")]
    Fit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FilterConfigSelectItems)]
pub(crate) enum StrokeWidthUnit {
    #[item(name = "SVG単位")]
    SvgUnit,
    #[item(name = "出力px")]
    OutputPx,
}

/// 線の描画進行の進め方
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FilterConfigSelectItems)]
pub(crate) enum TrimMode {
    /// 線ごとに同じ割合だけ描く
    #[item(name = "個別")]
    Individual,
    /// 文書順に 1 本ずつ描く
    #[item(name = "順番")]
    Sequential,
}

static FONT_DB: std::sync::LazyLock<std::sync::Arc<resvg::usvg::fontdb::Database>> =
    std::sync::LazyLock::new(|| {
        let mut db = resvg::usvg::fontdb::Database::new();
        db.load_system_fonts();
        std::sync::Arc::new(db)
    });

#[aviutl2::plugin(FilterPlugin)]
struct SvgFilter {}

#[aviutl2::filter::filter_config_items]
struct SvgConfig {
    #[track(name = "幅", range=1..=8192, default = 100, step = 1.0)]
    width: u32,
    #[track(name = "高さ", range=1..=8192, default = 100, step = 1.0)]
    height: u32,
    #[check(name = "アスペクト比の維持", default = true)]
    maintain_aspect_ratio: bool,
    #[select(name = "サイズ基準", items = SizeBasis, default = SizeBasis::Width)]
    size_basis: SizeBasis,
    #[file(name = "ファイル", filters = { "SVG" => ["svg", "svgz"] })]
    svg_file: Option<std::path::PathBuf>,
    #[color(name = "色", default = 0xffffff)]
    color: aviutl2::filter::FilterConfigColorValue,
    #[check(name = "塗りを上書き", default = false)]
    override_fill: bool,
    #[track(name = "塗り透明度", range = 0.0..=100.0, default = 100.0, step = 0.1)]
    fill_opacity: f64,
    #[select(name = "描画順", items = PaintOrder, default = PaintOrder::FillThenStroke)]
    paint_order: PaintOrder,
    #[group(name = "ストローク", opened = false)]
    stroke: group! {
        #[check(name = "ストロークを上書き", default = false)]
        stroke_enabled: bool,
        #[color(name = "線色", default = 0x000000)]
        stroke_color: aviutl2::filter::FilterConfigColorValue,
        #[track(name = "線幅", range = 0.0..=200.0, default = 1.0, step = 0.1)]
        stroke_width: f64,
        #[select(name = "線幅の単位", items = StrokeWidthUnit, default = StrokeWidthUnit::SvgUnit)]
        stroke_width_unit: StrokeWidthUnit,
        #[track(name = "線透明度", range = 0.0..=100.0, default = 100.0, step = 0.1)]
        stroke_opacity: f64,
        #[select(name = "線種", items = DashPreset, default = DashPreset::Solid)]
        dash_preset: DashPreset,
        #[string(name = "カスタム線種", default = "")]
        custom_dash: String,
        #[track(name = "線種オフセット", range = -1000.0..=1000.0, default = 0.0, step = 0.1)]
        dashoffset: f64,
        #[select(name = "端", items = StrokeLinecap, default = StrokeLinecap::Butt)]
        linecap: StrokeLinecap,
        #[select(name = "角", items = StrokeLinejoin, default = StrokeLinejoin::Miter)]
        linejoin: StrokeLinejoin,
        #[track(name = "マイター限界", range = 1.0..=100.0, default = 4.0, step = 0.1)]
        miterlimit: f64,
    },
    #[group(name = "線の描画進行", opened = false)]
    trim: group! {
        #[track(name = "描画開始", range = 0.0..=100.0, default = 0.0, step = 0.1)]
        trim_start: f64,
        #[track(name = "描画終了", range = 0.0..=100.0, default = 100.0, step = 0.1)]
        trim_end: f64,
        #[select(name = "進行方法", items = TrimMode, default = TrimMode::Individual)]
        trim_mode: TrimMode,
    },
    #[group(name = "クリッピング", opened = false)]
    clipping: group! {
        #[track(name = "左", range = 0.0..=8192.0, default = 0.0, step = 0.01)]
        clip_left: f64,
        #[track(name = "上", range = 0.0..=8192.0, default = 0.0, step = 0.01)]
        clip_top: f64,
        #[track(name = "右", range = 0.0..=8192.0, default = 0.0, step = 0.01)]
        clip_right: f64,
        #[track(name = "下", range = 0.0..=8192.0, default = 0.0, step = 0.01)]
        clip_bottom: f64,
    },
    #[group(name = "部分描画", opened = false)]
    part: group! {
        #[string(name = "要素ID", default = "")]
        element_id: String,
    },
    #[group(name = "インライン入力", opened = false)]
    inline_input: group! {
        #[text(name = "SVGコード", default = "")]
        svg_data: String,
    },
}

fn percent01(v: f64) -> f32 {
    (v / 100.0).clamp(0.0, 1.0) as f32
}

fn params_from_config(c: &SvgConfig) -> RenderParams {
    RenderParams {
        style: Style {
            color: c.color.to_rgb(),
            override_fill: c.override_fill,
            fill_opacity: percent01(c.fill_opacity),
            paint_order: c.paint_order,
            stroke: StrokeStyle {
                enabled: c.stroke_enabled,
                color: c.stroke_color.to_rgb(),
                width: c.stroke_width.max(0.0) as f32,
                width_unit: c.stroke_width_unit,
                opacity: percent01(c.stroke_opacity),
                dash_preset: c.dash_preset,
                custom_dash: rewrite::parse_dash(&c.custom_dash),
                dashoffset: c.dashoffset as f32,
                linecap: c.linecap,
                linejoin: c.linejoin,
                miterlimit: c.miterlimit.max(1.0) as f32,
            },
            trim: Trim {
                start: percent01(c.trim_start),
                end: percent01(c.trim_end),
                mode: c.trim_mode,
            },
            element_id: c.element_id.trim().to_string(),
        },
        layout: Layout {
            width: c.width,
            height: c.height,
            keep_aspect: c.maintain_aspect_ratio,
            basis: c.size_basis,
            clip: [
                c.clip_left as f32,
                c.clip_top as f32,
                c.clip_right as f32,
                c.clip_bottom as f32,
            ],
        },
    }
}

/// キャッシュキーに入れる「読み込み元」。ファイルは更新日時とサイズも入れ、
/// 外部で保存し直したら描き直す
#[derive(Hash)]
enum SourceIdent<'a> {
    File {
        path: &'a Path,
        modified: Option<std::time::SystemTime>,
        len: u64,
    },
    Missing(&'a Path),
    Inline(&'a str),
}

impl<'a> SourceIdent<'a> {
    fn of(source: &'a Source) -> Self {
        match source {
            Source::File(path) => match std::fs::metadata(path) {
                Ok(meta) => SourceIdent::File {
                    path,
                    modified: meta.modified().ok(),
                    len: meta.len(),
                },
                Err(_) => SourceIdent::Missing(path),
            },
            Source::Inline(data) => SourceIdent::Inline(data),
        }
    }
}

fn hash_of(value: &impl Hash) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

static REPORTED: LazyLock<Mutex<HashSet<u64>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// 同じ失敗を 1 回目だけ true にする。描画は毎フレーム呼ばれるので、毎回ログに出さない
fn first_report(key: u64) -> bool {
    REPORTED.lock().map(|mut set| set.insert(key)).unwrap_or(true)
}

fn load_and_render(source: &Source, params: &RenderParams) -> anyhow::Result<render::Rendered> {
    let (data, resources_dir) = render::load(source)?;
    render::render(&data, resources_dir.as_deref(), &FONT_DB, params)
        .with_context(|| format!("SVG_H: {source:?} を描けない"))
}

impl aviutl2::filter::FilterPlugin for SvgFilter {
    fn new(_info: aviutl2::AviUtl2Info) -> aviutl2::AnyResult<Self> {
        aviutl2::tracing_subscriber::fmt()
            .with_max_level(if cfg!(debug_assertions) {
                tracing::Level::DEBUG
            } else {
                tracing::Level::INFO
            })
            .event_format(aviutl2::logger::AviUtl2Formatter)
            .with_writer(aviutl2::logger::AviUtl2LogWriter)
            .init();
        Ok(Self {})
    }

    fn plugin_info(&self) -> aviutl2::filter::FilterPluginTable {
        aviutl2::filter::FilterPluginTable {
            name: "SVG_H".into(),
            label: None,
            flags: aviutl2::bitflag!(aviutl2::filter::FilterPluginFlags {
                video: true,
                input: true
            }),
            information: format!(
                "SVG_H Object, powered by resvg, written in Rust / v{version} / forked from sevenc-nanashi/svg.aux2",
                version = env!("CARGO_PKG_VERSION")
            ),
            config_items: SvgConfig::to_config_items(),
        }
    }

    fn proc_video(
        &self,
        config: &[aviutl2::filter::FilterConfigItem],
        video: &mut aviutl2::filter::FilterProcVideo,
    ) -> aviutl2::AnyResult<()> {
        let config = config.to_struct::<SvgConfig>();
        let source = match (config.svg_data.as_str(), config.svg_file.as_ref()) {
            (inline, _) if !inline.trim().is_empty() => Source::Inline(inline.to_string()),
            (_, Some(path)) => Source::File(path.clone()),
            _ => return Ok(()),
        };
        let params = params_from_config(&config);
        let ident = SourceIdent::of(&source);
        let cache_hash = format!("{CACHE_NS}{}", hash_of(&(&ident, &params)));

        if let Some(entry) =
            aviutl2::cache::get_image_cache(&aviutl2::cache::GLOBAL_CACHE_HANDLE, &cache_hash)?
        {
            video.set_image_data(entry.as_u8_slice(), entry.width() as _, entry.height() as _);
            return Ok(());
        }
        tracing::debug!("Rendering SVG {source:?} ({cache_hash})");

        // 失敗しても Err を返さない（ホストが毎フレーム ERROR を出す）。1 回だけ記録して代わりの絵を出す
        let image = match load_and_render(&source, &params) {
            Ok(image) => image,
            Err(e) => {
                let message = format!("{e:#}");
                if first_report(hash_of(&(&ident, &message))) {
                    tracing::error!("{message}");
                }
                render::placeholder(&params.layout)
            }
        };

        let mut entry = aviutl2::cache::create_image_cache(
            &aviutl2::cache::GLOBAL_CACHE_HANDLE,
            &cache_hash,
            image.width as _,
            image.height as _,
        )?;
        entry.as_u8_slice_mut().copy_from_slice(&image.rgba);
        video.set_image_data(entry.as_u8_slice(), entry.width() as _, entry.height() as _);
        Ok(())
    }
}

aviutl2::register_generic_plugin!(SvgHAux2);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_ident_follows_file_edits() {
        let path = std::env::temp_dir().join(format!("svg_h_ident_{}.svg", std::process::id()));
        std::fs::write(&path, "<svg/>").unwrap();
        let source = Source::File(path.clone());
        let before = hash_of(&SourceIdent::of(&source));
        std::fs::write(&path, "<svg width=\"1\"/>").unwrap();
        let after = hash_of(&SourceIdent::of(&source));
        std::fs::remove_file(&path).unwrap();
        let missing = hash_of(&SourceIdent::of(&source));
        assert_ne!(before, after);
        assert_ne!(after, missing);
    }

    #[test]
    fn report_only_once() {
        let key = hash_of(&"report_only_once");
        assert!(first_report(key));
        assert!(!first_report(key));
    }
}
