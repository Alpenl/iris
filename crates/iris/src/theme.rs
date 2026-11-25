//! Direction C "Modular panels" tokens and GPUI `Global` runtime theme engine.
//!
//! Detail rules follow Zed and Sooty: ghost buttons with no frame at rest,
//! one filled step for hover and another for selected, list rows without
//! separators, and text in three weights of grey.

use gpui::{App, Global, Pixels, Rgba, px, rgba};
use iris_api::UiThemeSpec;

pub(crate) const FONT_UI: &str = "Space Grotesk";
pub(crate) const FONT_MONO: &str = "IBM Plex Mono";

const TITLEBAR_H: f32 = 36.;
/// The control bar under the picture; floating in fullscreen.
const DOCK_H: f32 = 44.;
/// Padding for fullscreen overlays such as the media title and info panel.
pub(crate) const FLOAT_GAP: f32 = 16.;
const ROW_H: f32 = 28.;
pub(crate) const BUTTON: f32 = 28.;

const DEFAULT_CORNER_RADIUS: f32 = 0.0;
const MIN_TEXT_CONTRAST: f32 = 4.5;
const HIGH_CONTRAST_LIGHT: u32 = 0xF2F4F8FF;
const HIGH_CONTRAST_DARK: u32 = 0x111317FF;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BuiltinThemeInfo {
    pub(crate) id: &'static str,
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
}

pub(crate) const BUILTIN_THEMES: &[BuiltinThemeInfo] = &[
    BuiltinThemeInfo {
        id: "graphite",
        name: "石墨模块",
        description: "方向 C 默认硬朗深灰与信号橙 #FF5B1F",
    },
    BuiltinThemeInfo {
        id: "oled",
        name: "OLED 纯黑暗房",
        description: "纯黑 #000000 零漏光背景，适合暗室沉浸观影",
    },
    BuiltinThemeInfo {
        id: "studio",
        name: "胶片暖灰审片",
        description: "中性暖灰 #141312 与琥珀强调色 #E59934，适合摄影选片与审片",
    },
    BuiltinThemeInfo {
        id: "glacier",
        name: "冰川高对比",
        description: "深海冷灰 #0B0E14 与冰蓝强调色 #38BDF8",
    },
];

#[derive(Clone, Copy)]
struct ThemePalette {
    window_bg: u32,
    panel: u32,
    hover: u32,
    selected: u32,
    line: u32,
    line_strong: u32,
    text: u32,
    text_muted: u32,
    text_dim: u32,
    text_faint: u32,
    accent: u32,
    danger: u32,
    overlay: u32,
    scrim: u32,
}

const DANGER_DEFAULT: u32 = 0xF05252FF;

const GRAPHITE_PALETTE: ThemePalette = ThemePalette {
    window_bg: 0x0E0F11FF,
    panel: 0x15171AFF,
    hover: 0x1F2226FF,
    selected: 0x25292EFF,
    line: 0x2A2E33FF,
    line_strong: 0x3A3F46FF,
    text: 0xE6E8EBFF,
    text_muted: 0xB4B9BFFF,
    text_dim: 0x8D939AFF,
    text_faint: 0x5C6168FF,
    accent: 0xFF5B1FFF,
    danger: DANGER_DEFAULT,
    overlay: 0x15171AE6,
    scrim: 0x00000099,
};

const OLED_PALETTE: ThemePalette = ThemePalette {
    window_bg: 0x000000FF,
    panel: 0x08090BFF,
    hover: 0x121418FF,
    selected: 0x1A1D23FF,
    line: 0x1E2127FF,
    line_strong: 0x2E333BFF,
    text: 0xE0E2E6FF,
    text_muted: 0xA6ABB3FF,
    text_dim: 0x7A8088FF,
    text_faint: 0x4E535AFF,
    accent: 0xFF5B1FFF,
    danger: DANGER_DEFAULT,
    overlay: 0x08090BEB,
    scrim: 0x000000BB,
};

const STUDIO_PALETTE: ThemePalette = ThemePalette {
    window_bg: 0x141312FF,
    panel: 0x1C1A18FF,
    hover: 0x262421FF,
    selected: 0x302D29FF,
    line: 0x36322EFF,
    line_strong: 0x4A453FFF,
    text: 0xEAE6E1FF,
    text_muted: 0xB8B2AAFF,
    text_dim: 0x8F8981FF,
    text_faint: 0x635E58FF,
    accent: 0xE59934FF,
    danger: DANGER_DEFAULT,
    overlay: 0x1C1A18E6,
    scrim: 0x09080799,
};

const GLACIER_PALETTE: ThemePalette = ThemePalette {
    window_bg: 0x0B0E14FF,
    panel: 0x111620FF,
    hover: 0x19202EFF,
    selected: 0x222B3DFF,
    line: 0x263145FF,
    line_strong: 0x374661FF,
    text: 0xECF2FAFF,
    text_muted: 0xB6C2D4FF,
    text_dim: 0x8695ABFF,
    text_faint: 0x556378FF,
    accent: 0x38BDF8FF,
    danger: DANGER_DEFAULT,
    overlay: 0x111620E6,
    scrim: 0x04070CA6,
};

impl ThemePalette {
    fn for_id(id: &str) -> Self {
        let key = id.trim();
        if key.eq_ignore_ascii_case("oled") {
            OLED_PALETTE
        } else if key.eq_ignore_ascii_case("studio") {
            STUDIO_PALETTE
        } else if key.eq_ignore_ascii_case("glacier") {
            GLACIER_PALETTE
        } else {
            GRAPHITE_PALETTE
        }
    }
}

/// Precomputed GPUI `Global` visual palette and clamped layout metrics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ThemeTokens {
    pub(crate) window_bg: Rgba,
    pub(crate) panel: Rgba,
    pub(crate) hover: Rgba,
    pub(crate) selected: Rgba,
    pub(crate) line: Rgba,
    pub(crate) line_strong: Rgba,
    pub(crate) text: Rgba,
    pub(crate) text_muted: Rgba,
    pub(crate) text_dim: Rgba,
    pub(crate) text_faint: Rgba,
    pub(crate) accent: Rgba,
    pub(crate) danger: Rgba,
    pub(crate) overlay: Rgba,
    pub(crate) scrim: Rgba,
    pub(crate) radius_lg: Pixels,
    pub(crate) radius_md: Pixels,
    pub(crate) radius_sm: Pixels,
    pub(crate) titlebar_h: Pixels,
    pub(crate) dock_h: Pixels,
    pub(crate) row_h: Pixels,
}

impl Global for ThemeTokens {}

impl Default for ThemeTokens {
    fn default() -> Self {
        Self::resolve("graphite", None)
    }
}

impl ThemeTokens {
    /// Reads the active [`ThemeTokens`] from GPUI `App` state, falling back to Direction C graphite.
    #[inline]
    pub(crate) fn of(cx: &App) -> Self {
        cx.try_global::<Self>().copied().unwrap_or_default()
    }

    /// Resolves `base_id` merged with optional plugin [`UiThemeSpec`] overrides,
    /// clamping all metrics and enforcing a minimum luminance safety floor on `text`.
    pub(crate) fn resolve(base_id: &str, plugin_override: Option<&UiThemeSpec>) -> Self {
        let mut p = ThemePalette::for_id(base_id);
        let mut tb_h = TITLEBAR_H;
        let mut dk_h = DOCK_H;
        let mut rw_h = ROW_H;
        let mut radius = DEFAULT_CORNER_RADIUS;

        if let Some(spec) = plugin_override {
            apply_hex_override(&mut p.window_bg, spec.window_bg.as_deref());
            apply_hex_override(&mut p.panel, spec.panel.as_deref());
            apply_hex_override(&mut p.hover, spec.hover.as_deref());
            apply_hex_override(&mut p.selected, spec.selected.as_deref());
            apply_hex_override(&mut p.line, spec.line.as_deref());
            apply_hex_override(&mut p.line_strong, spec.line_strong.as_deref());
            apply_hex_override(&mut p.text, spec.text.as_deref());
            apply_hex_override(&mut p.text_muted, spec.text_muted.as_deref());
            apply_hex_override(&mut p.text_dim, spec.text_dim.as_deref());
            apply_hex_override(&mut p.text_faint, spec.text_faint.as_deref());
            apply_hex_override(&mut p.accent, spec.accent.as_deref());
            apply_hex_override(&mut p.overlay, spec.overlay.as_deref());
            apply_hex_override(&mut p.scrim, spec.scrim.as_deref());

            apply_metric_override(&mut tb_h, spec.titlebar_height, 28.0, 52.0);
            apply_metric_override(&mut dk_h, spec.dock_height, 34.0, 64.0);
            apply_metric_override(&mut rw_h, spec.row_height, 22.0, 40.0);
            apply_metric_override(&mut radius, spec.corner_radius, 0.0, 14.0);
        }

        if plugin_override.is_some() {
            p.window_bg |= 0xFF;
            // Pick a common foreground polarity. Independently accepting a black panel and
            // white window can leave no usable foreground shared by the two surfaces.
            let window = rgba(p.window_bg);
            let preferred =
                if contrast(rgba(0xFFFFFFFF), window) >= contrast(rgba(0x000000FF), window) {
                    HIGH_CONTRAST_LIGHT
                } else {
                    HIGH_CONTRAST_DARK
                };
            let extreme = if preferred == HIGH_CONTRAST_LIGHT {
                0xFFFFFFFF
            } else {
                0x000000FF
            };
            for surface in [&mut p.panel, &mut p.hover, &mut p.selected] {
                *surface |= 0xFF;
                if contrast(rgba(extreme), rgba(*surface)) < MIN_TEXT_CONTRAST {
                    *surface = p.window_bg;
                }
            }
            // The overlay can sit over arbitrary media, so test the worst case for the
            // fallback foreground as well as checking each foreground over both extremes.
            let mut overlay = rgba(p.overlay);
            let black = rgba(0x000000FF);
            let white = rgba(0xFFFFFFFF);
            if overlay.a < 0.9
                || [black, white]
                    .map(|bg| composite(overlay, bg))
                    .into_iter()
                    .any(|bg| contrast(rgba(extreme), bg) < MIN_TEXT_CONTRAST)
            {
                p.overlay = p.window_bg;
                overlay = window;
            }
            let surfaces = [
                window,
                rgba(p.panel),
                rgba(p.hover),
                rgba(p.selected),
                composite(overlay, black),
                composite(overlay, white),
            ];
            let curated = ThemePalette::for_id(base_id);
            for (slot, fallback) in [
                (&mut p.text, curated.text),
                (&mut p.text_muted, curated.text_muted),
                (&mut p.text_dim, curated.text_dim),
                (&mut p.text_faint, curated.text_faint),
                (&mut p.accent, curated.accent),
                (&mut p.danger, curated.danger),
            ] {
                *slot = [*slot, fallback, preferred, extreme]
                    .into_iter()
                    .find(|&value| {
                        let foreground = rgba(value);
                        foreground.a >= 0.9
                            && surfaces
                                .iter()
                                .all(|&bg| contrast(foreground, bg) >= MIN_TEXT_CONTRAST)
                    })
                    .unwrap_or(extreme);
            }
            for (slot, fallback, minimum) in [
                (&mut p.line, curated.line, 1.2),
                (&mut p.line_strong, curated.line_strong, 1.5),
            ] {
                *slot = [*slot, fallback, preferred, extreme]
                    .into_iter()
                    .find(|&value| {
                        let outline = rgba(value);
                        outline.a >= 0.9
                            && [window, rgba(p.panel)]
                                .into_iter()
                                .all(|bg| contrast(outline, bg) >= minimum)
                    })
                    .unwrap_or(extreme);
            }
            if rgba(p.scrim).a < 0.35 {
                p.scrim = curated.scrim;
            }
        }

        Self {
            window_bg: rgba(p.window_bg),
            panel: rgba(p.panel),
            hover: rgba(p.hover),
            selected: rgba(p.selected),
            line: rgba(p.line),
            line_strong: rgba(p.line_strong),
            text: rgba(p.text),
            text_muted: rgba(p.text_muted),
            text_dim: rgba(p.text_dim),
            text_faint: rgba(p.text_faint),
            accent: rgba(p.accent),
            danger: rgba(p.danger),
            overlay: rgba(p.overlay),
            scrim: rgba(p.scrim),
            radius_lg: px(radius.clamp(0.0, 14.0)),
            radius_md: px(radius.clamp(0.0, 6.0)),
            radius_sm: px(radius.clamp(0.0, 3.0)),
            titlebar_h: px(tb_h),
            dock_h: px(dk_h),
            row_h: px(rw_h),
        }
    }
}

fn linearize_srgb(channel: f32) -> f32 {
    let c = channel.clamp(0.0, 1.0);
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn relative_luminance(color: Rgba) -> f32 {
    0.2126 * linearize_srgb(color.r)
        + 0.7152 * linearize_srgb(color.g)
        + 0.0722 * linearize_srgb(color.b)
}

fn composite(foreground: Rgba, background: Rgba) -> Rgba {
    let a = foreground.a;
    Rgba {
        r: foreground.r * a + background.r * (1. - a),
        g: foreground.g * a + background.g * (1. - a),
        b: foreground.b * a + background.b * (1. - a),
        a: 1.,
    }
}

fn contrast(foreground: Rgba, background: Rgba) -> f32 {
    let a = relative_luminance(composite(foreground, background));
    let b = relative_luminance(background);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn parse_hex_packed(s: &str) -> Option<u32> {
    let hex = s.strip_prefix('#')?;
    match hex.len() {
        6 if hex.bytes().all(|b| b.is_ascii_hexdigit()) => {
            let rgb = u32::from_str_radix(hex, 16).ok()?;
            Some((rgb << 8) | 0xFF)
        }
        8 if hex.bytes().all(|b| b.is_ascii_hexdigit()) => u32::from_str_radix(hex, 16).ok(),
        _ => None,
    }
}

/// Parses a `#RRGGBB` or `#RRGGBBAA` hex string (case-insensitive) into [`Rgba`].
pub(crate) fn parse_hex_rgba(s: &str) -> Option<Rgba> {
    parse_hex_packed(s).map(rgba)
}

fn apply_hex_override(slot: &mut u32, hex: Option<&str>) {
    if let Some(s) = hex
        && let Some(packed) = parse_hex_packed(s)
    {
        *slot = packed;
    }
}

fn apply_metric_override(slot: &mut f32, value: Option<f32>, min: f32, max: f32) {
    if let Some(v) = value
        && v.is_finite()
    {
        *slot = v.clamp(min, max);
    }
}

#[cfg(test)]
mod tests {
    use gpui::{px, rgb, rgba};
    use iris_api::UiThemeSpec;

    use super::{
        BUILTIN_THEMES, DOCK_H, MIN_TEXT_CONTRAST, OLED_PALETTE, ROW_H, TITLEBAR_H, ThemeTokens,
        contrast, parse_hex_rgba,
    };

    #[test]
    fn parses_hex_rgb_and_rgba_case_insensitively() {
        assert_eq!(parse_hex_rgba("#FF5B1F"), Some(rgb(0xFF5B1F)));
        assert_eq!(parse_hex_rgba("#ff5b1f"), Some(rgb(0xFF5B1F)));
        assert_eq!(parse_hex_rgba("#15171AE6"), Some(rgba(0x15171AE6)));
        assert_eq!(parse_hex_rgba("#15171ae6"), Some(rgba(0x15171AE6)));
        assert_eq!(parse_hex_rgba("#00000000"), Some(rgba(0x00000000)));

        for invalid in [
            "",
            "FF5B1F",
            "#FFF",
            "#FFFF",
            "#12345",
            "#1234567",
            "#123456789",
            "#GGGGGG",
            "#+12345",
            "#12345G",
        ] {
            assert_eq!(parse_hex_rgba(invalid), None, "expected None for {invalid}");
        }
    }

    #[test]
    fn graphite_default_matches_direction_c_hex_spec_and_curated_themes() {
        let ids: Vec<&str> = BUILTIN_THEMES.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec!["graphite", "oled", "studio", "glacier"]);

        let g = ThemeTokens::default();
        assert_eq!(g, ThemeTokens::resolve("graphite", None));
        assert_eq!(g, ThemeTokens::resolve("unknown-theme", None));
        assert_eq!(g.window_bg, rgba(0x0E0F11FF));
        assert_eq!(g.panel, rgba(0x15171AFF));
        assert_eq!(g.hover, rgba(0x1F2226FF));
        assert_eq!(g.selected, rgba(0x25292EFF));
        assert_eq!(g.line, rgba(0x2A2E33FF));
        assert_eq!(g.line_strong, rgba(0x3A3F46FF));
        assert_eq!(g.text, rgba(0xE6E8EBFF));
        assert_eq!(g.text_muted, rgba(0xB4B9BFFF));
        assert_eq!(g.text_dim, rgba(0x8D939AFF));
        assert_eq!(g.text_faint, rgba(0x5C6168FF));
        assert_eq!(g.accent, rgba(0xFF5B1FFF));
        assert_eq!(g.overlay, rgba(0x15171AE6));
        assert_eq!(g.scrim, rgba(0x00000099));
        assert_eq!(g.radius_lg, px(0.0));
        assert_eq!(g.radius_md, px(0.0));
        assert_eq!(g.radius_sm, px(0.0));
        assert_eq!(g.titlebar_h, px(TITLEBAR_H));
        assert_eq!(g.dock_h, px(DOCK_H));
        assert_eq!(g.row_h, px(ROW_H));

        assert_eq!(ThemeTokens::resolve("oled", None).window_bg, rgb(0x000000));
        assert_eq!(ThemeTokens::resolve("studio", None).accent, rgb(0xE59934));
        assert_eq!(ThemeTokens::resolve("glacier", None).accent, rgb(0x38BDF8));
    }

    #[test]
    fn clamps_extreme_and_non_finite_metrics_table_driven() {
        // (input_radius, input_tb, input_dock, input_row, exp_r_lg, exp_r_md, exp_r_sm, exp_tb, exp_dock, exp_row)
        let cases = [
            (-50.0, -10.0, -99.0, -5.0, 0.0, 0.0, 0.0, 28.0, 34.0, 22.0),
            (999.0, 999.0, 999.0, 999.0, 14.0, 6.0, 3.0, 52.0, 64.0, 40.0),
            (
                f32::NAN,
                f32::NAN,
                f32::INFINITY,
                f32::NEG_INFINITY,
                0.0,
                0.0,
                0.0,
                TITLEBAR_H,
                DOCK_H,
                ROW_H,
            ),
            (5.0, 40.0, 48.0, 30.0, 5.0, 5.0, 3.0, 40.0, 48.0, 30.0),
        ];
        for (r, tb, dk, rw, exp_lg, exp_md, exp_sm, exp_tb, exp_dk, exp_rw) in cases {
            let spec = UiThemeSpec {
                corner_radius: Some(r),
                titlebar_height: Some(tb),
                dock_height: Some(dk),
                row_height: Some(rw),
                hover: Some("not-a-hex".into()),
                ..Default::default()
            };
            let t = ThemeTokens::resolve("oled", Some(&spec));
            assert_eq!(t.hover, rgba(OLED_PALETTE.hover));
            assert_eq!(t.radius_lg, px(exp_lg));
            assert_eq!(t.radius_md, px(exp_md));
            assert_eq!(t.radius_sm, px(exp_sm));
            assert_eq!(t.titlebar_h, px(exp_tb));
            assert_eq!(t.dock_h, px(exp_dk));
            assert_eq!(t.row_h, px(exp_rw));
        }
    }

    #[test]
    fn conflicting_or_transparent_theme_colors_preserve_text_contrast() {
        for (window, panel) in [("#00000000", "#FFFFFF"), ("#FFFFFF00", "#000000")] {
            let spec = UiThemeSpec {
                window_bg: Some(window.into()),
                panel: Some(panel.into()),
                hover: Some(panel.into()),
                selected: Some(panel.into()),
                overlay: Some("#00000000".into()),
                text: Some("#FFFFFF00".into()),
                text_muted: Some("#00000000".into()),
                text_dim: Some("#FFFFFF00".into()),
                text_faint: Some("#FFFFFF00".into()),
                accent: Some("#FFFFFF00".into()),
                ..Default::default()
            };
            let resolved = ThemeTokens::resolve("graphite", Some(&spec));
            assert_eq!(resolved.window_bg.a, 1.);
            for fg in [
                resolved.text,
                resolved.text_muted,
                resolved.text_dim,
                resolved.text_faint,
                resolved.accent,
            ] {
                assert!(fg.a >= 0.9);
                for bg in [
                    resolved.window_bg,
                    resolved.panel,
                    resolved.hover,
                    resolved.selected,
                    resolved.overlay,
                ] {
                    assert!(contrast(fg, bg) >= MIN_TEXT_CONTRAST);
                }
            }
        }
    }
}
