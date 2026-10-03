# svg_h.aux2

[日本語](README.md) | English

SVG renderer for AviUtl2 with stroke-style extensions. Fork of [sevenc-nanashi/svg.aux2](https://github.com/sevenc-nanashi/svg.aux2) (by Nanashi, MIT License).

## Requirements

- AviUtl ExEdit2 **beta46 or later** (minimum supported by [aviutl2-rs](https://github.com/sevenc-nanashi/aviutl2-rs) 0.29, the SDK used). Tested on 2.1.11a
- [GCMZDrops2](https://github.com/oov/aviutl2_gcmzdrops2) only if you use the drop handler (the plugin itself works without it)

## Installation

1. Download the zip from [Releases](https://github.com/HexBrowns/svg_h/releases) and extract it
2. Copy the `Plugin` and `Language` folders in it into the AviUtl2 data folder (`C:\ProgramData\aviutl2` by default). This places three files:
   - `Plugin\svg_h\svg_h.aux2` (the plugin)
   - `Language\English.svg_h.aul2` (English display names)
   - `Plugin\GCMZDrops\GCMZScript\svg_h2obj.lua` (drop handler for GCMZDrops2; unused if GCMZDrops2 is not installed)
3. Restart AviUtl2 (files copied while it is running are not loaded until the restart)
4. Place `SVG_H` and choose an SVG file in `File`

It can be installed side by side with the upstream `svg.aux2` (see the identifier table below).

> [!TIP]
> This plugin re-renders the SVG whenever width or height parameters change.
> For resize animations, keep width/height fixed and scale with other filter effects.

## Identifier changes (coexistence with svg.aux2)

| Item | Upstream | This fork |
|------|----------|-----------|
| Package id | `sevenc-nanashi.svg-aux2` | `svg-h-aux2` |
| Plugin file | `Plugin/svg.aux2` | `Plugin/svg_h/svg_h.aux2` |
| Filter name | `SVG` | `SVG_H` |
| Drop handler | `svg.aux2` | `svg_h.aux2` |
| Cache namespace | (none) | `svg_h:v2:` |

## Additional settings

| Setting | Behavior |
|---|---|
| Size Basis | With "Keep Aspect Ratio": `Width` (default) / `Height` / `Fit in Box` |
| Override Fill | Recolors visible fills only; `fill="none"` stays unfilled |
| Fill / Stroke Opacity | Multiplied with the SVG's own opacity |
| Paint Order | `Stroke over Fill` keeps the SVG as is; `Fill over Stroke` forces it |
| Stroke Width Unit | `SVG Units` or `Output px` (on-screen thickness) |
| Stroke Trim | Draw strokes progressively (`Trim Start` / `Trim End` in %, `Individually` or `Sequentially`) |
| Clipping | Crops in SVG units (fractions allowed) |
| Element ID | Renders only the element with that id, in place |

`.svgz` and relative image links are supported. Edited files are reloaded automatically.
Load errors show a red cross and are logged once. See `README.md` for details and for the
visual changes in 0.8.0 (straight alpha, SVG attributes respected, clipping, viewBox).

The GCMZDrops2 handler `svg_h2obj.lua` lives in `assets/GCMZScript/` and is deployed by `build.ps1`.
It turns `.svg` / `.svgz` into `SVG_H` objects and leaves every other file type (such as `.txt`) to the other handlers.

## Build

```powershell
.\build.ps1
.\build.ps1 -HandlerOnly   # deploy the GCMZDrops handler only
cargo test --lib
```

## License

MIT License (same as upstream). Upstream author: sevenc-nanashi.
