//! Visual tokens from BRIEF section 6 and the egui style built from them.

use std::sync::Arc;

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle};

pub const BG: [u8; 3] = [0x0B, 0x0D, 0x12];
pub const PANEL: [u8; 3] = [0x12, 0x16, 0x1E];
pub const PANEL_2: [u8; 3] = [0x14, 0x18, 0x21];
pub const BORDER: [u8; 3] = [0x23, 0x28, 0x34];
pub const TEXT: [u8; 3] = [0xE8, 0xEA, 0xF0];
pub const TEXT_2: [u8; 3] = [0x9A, 0xA1, 0xB0];
pub const NEEDS_YOU: [u8; 3] = [0x8C, 0xD3, 0xFF];
pub const WORKING: [u8; 3] = [0xF2, 0xB4, 0x5A];
pub const SYSTEM: [u8; 3] = [0x9B, 0x8C, 0xF0];
pub const SYSTEM_2: [u8; 3] = [0xB7, 0xA6, 0xFF];
pub const RESTING: [u8; 3] = [0x5B, 0x61, 0x70];
pub const TERMINAL_BG: [u8; 3] = [0x0E, 0x11, 0x17];

pub fn c(rgb: [u8; 3]) -> Color32 {
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

pub fn ca(rgb: [u8; 3], alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(rgb[0], rgb[1], rgb[2], alpha)
}

/// sRGB bytes to linear floats for the renderer.
pub fn linear(rgb: [u8; 3]) -> [f32; 3] {
    rgb.map(|v| meerora_geom::srgb_to_linear(v as f32 / 255.0))
}

pub fn hex_rgb(hex: &str) -> Option<[u8; 3]> {
    let s = hex.strip_prefix('#').unwrap_or(hex);
    if s.len() != 6 {
        return None;
    }
    let p = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
    Some([p(0)?, p(2)?, p(4)?])
}

/// UI font, Bricolage Grotesque.
pub fn ui(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn ui_semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

pub fn ui_bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}

/// Labels and status, IBM Plex Mono.
pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

pub fn mono_medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("mono-medium".into()))
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let add = |fonts: &mut FontDefinitions, name: &str, bytes: &'static [u8]| {
        fonts.font_data.insert(name.into(), Arc::new(FontData::from_static(bytes)));
    };
    add(&mut fonts, "bricolage", include_bytes!("../../../assets/fonts/bricolage-grotesque-400.ttf"));
    add(&mut fonts, "bricolage-600", include_bytes!("../../../assets/fonts/bricolage-grotesque-600.ttf"));
    add(&mut fonts, "bricolage-700", include_bytes!("../../../assets/fonts/bricolage-grotesque-700.ttf"));
    add(&mut fonts, "plex-mono", include_bytes!("../../../assets/fonts/ibm-plex-mono-400.ttf"));
    add(&mut fonts, "plex-mono-500", include_bytes!("../../../assets/fonts/ibm-plex-mono-500.ttf"));

    // egui's bundled fonts stay behind ours as fallbacks for missing glyphs.
    let fallback_prop = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let fallback_mono = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let with = |first: &str, rest: &[String]| std::iter::once(first.to_string()).chain(rest.iter().cloned()).collect::<Vec<_>>();
    fonts.families.insert(FontFamily::Proportional, with("bricolage", &fallback_prop));
    fonts.families.insert(FontFamily::Name("semibold".into()), with("bricolage-600", &fallback_prop));
    fonts.families.insert(FontFamily::Name("bold".into()), with("bricolage-700", &fallback_prop));
    fonts.families.insert(FontFamily::Monospace, with("plex-mono", &fallback_mono));
    fonts.families.insert(FontFamily::Name("mono-medium".into()), with("plex-mono-500", &fallback_mono));
    ctx.set_fonts(fonts);

    ctx.set_visuals(egui::Visuals::dark());
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, ui(12.0)),
            (TextStyle::Body, ui(15.0)),
            (TextStyle::Button, ui(14.0)),
            (TextStyle::Heading, ui_bold(18.0)),
            (TextStyle::Monospace, mono(13.0)),
        ]
        .into();
        let v = &mut style.visuals;
        v.panel_fill = c(PANEL);
        v.window_fill = c(PANEL);
        v.extreme_bg_color = c(TERMINAL_BG);
        v.faint_bg_color = c(PANEL_2);
        v.override_text_color = Some(c(TEXT));
        v.window_stroke = Stroke::new(1.0, c(BORDER));
        v.window_corner_radius = CornerRadius::same(14);
        v.selection.bg_fill = ca(NEEDS_YOU, 80);
        for w in
            [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.noninteractive, &mut v.widgets.open]
        {
            w.corner_radius = CornerRadius::same(10);
        }
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, c(BORDER));
        v.widgets.inactive.weak_bg_fill = c(PANEL_2);
        v.widgets.inactive.bg_fill = c(PANEL_2);
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, c([0x26, 0x2C, 0x38]));
        v.widgets.hovered.weak_bg_fill = c([0x1A, 0x1F, 0x2A]);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, c([0x3A, 0x42, 0x52]));
        v.widgets.active.weak_bg_fill = c([0x1E, 0x23, 0x2E]);
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.interaction.selectable_labels = false;
    });
}
