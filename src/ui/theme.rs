//! Design system: colours, fonts, global style and the few custom widgets
//! the UI is built from (buttons, avatars, toggles, segmented controls…).

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, Rect, Response, RichText, Sense, Shadow, Stroke,
    StrokeKind, Vec2, pos2, vec2,
};

// ---------------------------------------------------------------- palette

pub const BG: Color32 = Color32::from_rgb(0x17, 0x18, 0x1C);
pub const SIDEBAR: Color32 = Color32::from_rgb(0x10, 0x11, 0x14);
pub const CARD: Color32 = Color32::from_rgb(0x1F, 0x21, 0x26);
pub const CARD_HOVER: Color32 = Color32::from_rgb(0x27, 0x2A, 0x31);
pub const ELEVATED: Color32 = Color32::from_rgb(0x2A, 0x2D, 0x35);
pub const ELEVATED_HOVER: Color32 = Color32::from_rgb(0x34, 0x38, 0x42);
pub const INPUT: Color32 = Color32::from_rgb(0x0E, 0x0F, 0x12);
pub const BORDER: Color32 = Color32::from_rgb(0x2E, 0x31, 0x39);
pub const TEXT: Color32 = Color32::from_rgb(0xEC, 0xEE, 0xF2);
pub const MUTED: Color32 = Color32::from_rgb(0x98, 0x9E, 0xAA);
pub const FAINT: Color32 = Color32::from_rgb(0x62, 0x68, 0x73);
pub const ACCENT: Color32 = Color32::from_rgb(0x6E, 0x62, 0xF6);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0x80, 0x75, 0xFF);
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(0x2B, 0x28, 0x4D);
pub const LIVE: Color32 = Color32::from_rgb(0xEF, 0x44, 0x52);
pub const LIVE_HOVER: Color32 = Color32::from_rgb(0xFF, 0x5A, 0x67);
pub const ONLINE: Color32 = Color32::from_rgb(0x3B, 0xA5, 0x5D);

pub const RADIUS: u8 = 10;

// ---------------------------------------------------------------- icons (Segoe Fluent / MDL2 Assets)

pub mod icon {
    pub const ADD_FRIEND: char = '\u{E8FA}';
    pub const PEOPLE: char = '\u{E716}';
    pub const SETTINGS: char = '\u{E713}';
    pub const MONITOR: char = '\u{E7F4}';
    pub const WINDOW: char = '\u{E737}';
    pub const CAMERA: char = '\u{E722}';
    pub const PLAY: char = '\u{E768}';
    pub const STOP: char = '\u{E71A}';
    pub const COPY: char = '\u{E8C8}';
    pub const REFRESH: char = '\u{E72C}';
    pub const DELETE: char = '\u{E74D}';
    pub const FOLDER: char = '\u{E8B7}';
    pub const CHECK: char = '\u{E73E}';
    pub const CLOSE: char = '\u{E711}';
    pub const FULLSCREEN: char = '\u{E740}';
    pub const EXIT_FULLSCREEN: char = '\u{E73F}';
    pub const VOLUME: char = '\u{E767}';
    pub const MUTE: char = '\u{E74F}';
    pub const DOWNLOAD: char = '\u{E896}';
    pub const SEND: char = '\u{E724}';
    pub const INFO: char = '\u{E946}';
}

// ---------------------------------------------------------------- fonts & style

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("icons".into()))
}

pub fn icon_text(c: char, size: f32) -> RichText {
    RichText::new(c.to_string()).font(icon_font(size))
}

pub fn title(text: &str) -> RichText {
    RichText::new(text).font(semibold(24.0)).color(TEXT)
}

pub fn strong(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text).font(semibold(size)).color(TEXT)
}

pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).color(MUTED)
}

/// Windows' own UI fonts (always present), with emoji/symbol fallbacks for
/// names. Nothing is bundled in the exe.
pub fn install(ctx: &egui::Context) {
    use egui::{FontData, FontDefinitions};
    let mut fonts = FontDefinitions::default();
    let dir = std::path::PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| r"C:\Windows".into())).join("Fonts");
    let mut load = |key: &str, files: &[&str]| -> bool {
        for f in files {
            if let Ok(bytes) = std::fs::read(dir.join(f)) {
                fonts.font_data.insert(key.into(), std::sync::Arc::new(FontData::from_owned(bytes)));
                return true;
            }
        }
        false
    };
    let regular = load("ui", &["segoeui.ttf"]);
    let semi = load("ui-semibold", &["seguisb.ttf", "segoeuib.ttf"]);
    let icons = load("icons", &["SegoeIcons.ttf", "segmdl2.ttf"]);
    let emoji = load("emoji", &["seguiemj.ttf"]);
    let symbols = load("symbols", &["seguisym.ttf"]);

    let defaults = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut fallbacks: Vec<String> = vec![];
    if emoji {
        fallbacks.push("emoji".into());
    }
    if symbols {
        fallbacks.push("symbols".into());
    }
    if icons {
        fallbacks.push("icons".into());
    }
    let family = |first: Option<&str>| -> Vec<String> {
        let mut v: Vec<String> = first.map(|s| vec![s.to_string()]).unwrap_or_default();
        v.extend(defaults.iter().cloned());
        v.extend(fallbacks.iter().cloned());
        v
    };
    let prop = family(regular.then_some("ui"));
    let semibold = family(if semi { Some("ui-semibold") } else if regular { Some("ui") } else { None });
    let icon_family = family(icons.then_some("icons"));
    fonts.families.insert(FontFamily::Proportional, prop);
    fonts.families.insert(FontFamily::Name("semibold".into()), semibold);
    fonts.families.insert(FontFamily::Name("icons".into()), icon_family);
    if let Some(mono) = fonts.families.get_mut(&FontFamily::Monospace) {
        mono.extend(fallbacks.iter().cloned());
    }
    ctx.set_fonts(fonts);
    apply_style(ctx);
}

fn apply_style(ctx: &egui::Context) {
    use egui::{TextStyle, Theme};
    ctx.set_theme(Theme::Dark);
    let mut style = (*ctx.style_of(Theme::Dark)).clone();
    style.text_styles = [
        (TextStyle::Small, FontId::proportional(12.0)),
        (TextStyle::Body, FontId::proportional(14.5)),
        (TextStyle::Button, FontId::proportional(14.5)),
        (TextStyle::Heading, semibold(20.0)),
        (TextStyle::Monospace, FontId::monospace(13.0)),
    ]
    .into();
    let s = &mut style.spacing;
    s.item_spacing = vec2(8.0, 8.0);
    s.button_padding = vec2(12.0, 6.0);
    s.interact_size = vec2(40.0, 30.0);
    s.window_margin = Margin::same(20);
    s.menu_margin = Margin::same(6);
    s.combo_width = 200.0;
    s.slider_rail_height = 4.0;

    let v = &mut style.visuals;
    v.dark_mode = true;
    v.panel_fill = BG;
    v.window_fill = ELEVATED;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_corner_radius = CornerRadius::same(14);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_shadow = Shadow { offset: [0, 10], blur: 30, spread: 0, color: Color32::from_black_alpha(140) };
    v.popup_shadow = Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(120) };
    v.extreme_bg_color = INPUT;
    v.faint_bg_color = CARD;
    v.hyperlink_color = ACCENT_HOVER;
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    v.slider_trailing_fill = true;
    v.interact_cursor = Some(egui::CursorIcon::PointingHand);
    v.collapsing_header_frame = false;
    let r = CornerRadius::same(8);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = CARD;
    w.noninteractive.weak_bg_fill = CARD;
    w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    w.noninteractive.corner_radius = r;
    for (wv, fill) in [(&mut w.inactive, ELEVATED), (&mut w.hovered, ELEVATED_HOVER), (&mut w.active, ELEVATED_HOVER), (&mut w.open, ELEVATED_HOVER)] {
        wv.bg_fill = fill;
        wv.weak_bg_fill = fill;
        wv.bg_stroke = Stroke::NONE;
        wv.fg_stroke = Stroke::new(1.5, TEXT);
        wv.corner_radius = r;
        wv.expansion = 0.0;
    }
    w.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x44, 0x48, 0x54));
    w.inactive.fg_stroke = Stroke::new(1.5, Color32::from_rgb(0xD4, 0xD7, 0xDE));
    ctx.set_style_of(Theme::Dark, style);
}

// ---------------------------------------------------------------- widgets

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Primary,
    Secondary,
    Danger,
    Ghost,
    Live,
}

/// Button with an optional icon, painted by hand for full control.
pub fn button(ui: &mut egui::Ui, kind: Kind, icon: Option<char>, text: &str, height: f32, enabled: bool) -> Response {
    button_w(ui, kind, icon, text, height, enabled, None)
}

/// Same as [`button`], stretched to `width` (e.g. full-width).
pub fn button_w(
    ui: &mut egui::Ui,
    kind: Kind,
    icon: Option<char>,
    text: &str,
    height: f32,
    enabled: bool,
    width: Option<f32>,
) -> Response {
    let font = semibold(if height >= 44.0 { 15.5 } else { 14.0 });
    let (fill, hover, fg) = match kind {
        Kind::Primary => (ACCENT, ACCENT_HOVER, Color32::WHITE),
        Kind::Secondary => (ELEVATED, ELEVATED_HOVER, TEXT),
        Kind::Danger => (Color32::from_rgb(0x3A, 0x1E, 0x23), Color32::from_rgb(0x4A, 0x23, 0x2A), LIVE_HOVER),
        Kind::Ghost => (Color32::TRANSPARENT, ELEVATED, MUTED),
        Kind::Live => (LIVE, LIVE_HOVER, Color32::WHITE),
    };
    let text_g = ui.painter().layout_no_wrap(text.to_string(), font, fg);
    let icon_g = icon.map(|c| ui.painter().layout_no_wrap(c.to_string(), icon_font(height * 0.36), fg));
    let pad = if height >= 44.0 { 22.0 } else { 14.0 };
    let gap = if text.is_empty() { 0.0 } else { 8.0 };
    let content_w = text_g.size().x + icon_g.as_ref().map_or(0.0, |g| g.size().x + gap);
    let size = vec2(width.unwrap_or(content_w + pad * 2.0), height);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let hovered = enabled && resp.hovered();
        let mut bg = if hovered { hover } else { fill };
        if !enabled {
            bg = bg.gamma_multiply(0.45);
        }
        if enabled && resp.is_pointer_button_down_on() {
            bg = bg.gamma_multiply(0.9);
        }
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::same(RADIUS), bg);
        let alpha = if enabled { 1.0 } else { 0.5 };
        let mut x = rect.center().x - content_w / 2.0;
        if let Some(g) = icon_g {
            p.galley(pos2(x, rect.center().y - g.size().y / 2.0), g.clone(), fg.gamma_multiply(alpha));
            x += g.size().x + gap;
        }
        p.galley(pos2(x, rect.center().y - text_g.size().y / 2.0), text_g, fg.gamma_multiply(alpha));
    }
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// Square, frameless icon button.
pub fn icon_button(ui: &mut egui::Ui, icon: char, tooltip: &str) -> Response {
    icon_button_sized(ui, icon, tooltip, 32.0, MUTED)
}

pub fn icon_button_sized(ui: &mut egui::Ui, icon: char, tooltip: &str, size: f32, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered();
        if hovered {
            ui.painter().rect_filled(rect, CornerRadius::same(8), ELEVATED_HOVER);
        }
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            icon,
            icon_font(size * 0.47),
            if hovered { TEXT } else { color },
        );
    }
    resp.on_hover_text(tooltip).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Deterministic, pleasant colour per person.
pub fn avatar_color(id: &str) -> Color32 {
    const COLORS: [Color32; 8] = [
        Color32::from_rgb(0x6E, 0x62, 0xF6),
        Color32::from_rgb(0xE0, 0x6C, 0x9F),
        Color32::from_rgb(0x2E, 0xA0, 0x8F),
        Color32::from_rgb(0xE8, 0x8B, 0x3C),
        Color32::from_rgb(0x3D, 0x8B, 0xE8),
        Color32::from_rgb(0x9B, 0x5C, 0xF6),
        Color32::from_rgb(0x4E, 0xA8, 0x4E),
        Color32::from_rgb(0xD9, 0x53, 0x53),
    ];
    let h = id.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    COLORS[(h % COLORS.len() as u32) as usize]
}

pub fn initials(name: &str) -> String {
    let mut words = name.split_whitespace();
    let a = words.next().and_then(|w| w.chars().next());
    let b = words.next().and_then(|w| w.chars().next());
    [a, b].into_iter().flatten().flat_map(|c| c.to_uppercase()).collect()
}

/// Round avatar with initials and an optional status dot.
pub fn avatar(ui: &mut egui::Ui, id: &str, name: &str, size: f32, status: Option<Color32>, ring: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    paint_avatar(ui.painter(), rect, id, name, status, ring);
    resp
}

pub fn paint_avatar(p: &egui::Painter, rect: Rect, id: &str, name: &str, status: Option<Color32>, ring: Color32) {
    let size = rect.width();
    let c = rect.center();
    p.circle_filled(c, size / 2.0, avatar_color(id));
    p.text(c, egui::Align2::CENTER_CENTER, initials(name), semibold(size * 0.4), Color32::WHITE);
    if let Some(s) = status {
        let r = (size * 0.16).max(4.5);
        let dc = rect.right_bottom() - vec2(r * 0.9, r * 0.9);
        p.circle_filled(dc, r + 2.5, ring);
        p.circle_filled(dc, r, s);
    }
}

/// iOS-style switch.
pub fn toggle(ui: &mut egui::Ui, on: &mut bool) -> Response {
    let size = vec2(40.0, 22.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_with_time(resp.id, *on, 0.12);
    if ui.is_rect_visible(rect) {
        let bg = lerp_color(ELEVATED_HOVER, ACCENT, t);
        ui.painter().rect_filled(rect, CornerRadius::same(11), bg);
        let x = egui::lerp(rect.left() + 11.0..=rect.right() - 11.0, t);
        ui.painter().circle_filled(pos2(x, rect.center().y), 8.0, Color32::WHITE);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// Pill-shaped segmented control. Returns true when the value changed.
pub fn segmented<T: PartialEq + Copy>(ui: &mut egui::Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let font = semibold(13.5);
    let galleys: Vec<_> = options.iter().map(|(_, l)| ui.painter().layout_no_wrap(l.to_string(), font.clone(), TEXT)).collect();
    let pad = 14.0;
    let widths: Vec<f32> = galleys.iter().map(|g| g.size().x + pad * 2.0).collect();
    let h = 32.0;
    let (rect, _) = ui.allocate_exact_size(vec2(widths.iter().sum::<f32>() + 6.0, h + 6.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(RADIUS), INPUT);
    let mut x = rect.left() + 3.0;
    let mut changed = false;
    for (i, (opt, _)) in options.iter().enumerate() {
        let r = Rect::from_min_size(pos2(x, rect.top() + 3.0), vec2(widths[i], h));
        let resp = ui.interact(r, ui.scope_id().with(("seg", i, rect.left() as i32, rect.top() as i32)), Sense::click());
        let selected = *value == *opt;
        if selected {
            ui.painter().rect_filled(r, CornerRadius::same(8), ACCENT);
        } else if resp.hovered() {
            ui.painter().rect_filled(r, CornerRadius::same(8), ELEVATED);
        }
        let color = if selected { Color32::WHITE } else if resp.hovered() { TEXT } else { MUTED };
        let g = &galleys[i];
        ui.painter().galley(r.center() - g.size() / 2.0, g.clone(), color);
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !selected {
            *value = *opt;
            changed = true;
        }
        x += widths[i];
    }
    changed
}

/// Rounded surface for grouping content.
pub fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// Small uppercase section label.
pub fn section(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text.to_uppercase()).font(semibold(11.5)).color(FAINT));
}

/// Small coloured pill (e.g. "EN DIRECT").
pub fn badge(ui: &mut egui::Ui, text: &str, fill: Color32, fg: Color32) -> Response {
    let g = ui.painter().layout_no_wrap(text.to_string(), semibold(10.5), fg);
    let (rect, resp) = ui.allocate_exact_size(g.size() + vec2(12.0, 4.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(5), fill);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
    resp
}

/// Clickable full-width row of fixed height with hover highlight. The row is
/// registered before its content so inner buttons stay clickable.
pub fn row<R>(ui: &mut egui::Ui, height: f32, selected: bool, add: impl FnOnce(&mut egui::Ui) -> R) -> (Response, R) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    let fill = if selected {
        ELEVATED
    } else if resp.contains_pointer() {
        CARD_HOVER
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    let inner = ui
        .scope_builder(
            egui::UiBuilder::new()
                .max_rect(rect.shrink2(vec2(8.0, 0.0)))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            add,
        )
        .inner;
    (resp, inner)
}

/// Single-line text truncated with "…" to `max_width`.
pub fn truncated(ui: &mut egui::Ui, text: &str, font: FontId, color: Color32, max_width: f32) -> Response {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = egui::text::TextWrapping {
        max_width,
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    let g = ui.painter().layout_job(job);
    let (rect, resp) = ui.allocate_exact_size(g.size(), Sense::hover());
    ui.painter().galley(rect.min, g, color);
    resp
}

/// Painted outline used for keyboard-free "selected" states.
pub fn outline(ui: &egui::Ui, rect: Rect, color: Color32, width: f32, radius: u8) {
    ui.painter().rect_stroke(rect, CornerRadius::same(radius), Stroke::new(width, color), StrokeKind::Inside);
}
