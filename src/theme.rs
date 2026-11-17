//! Nicord visual theme: Discord-like dark UI + bundled Inter font.
//!
//! Font: **Inter** (OFL, by Rasmus Andersson — the open font deliberately
//! designed to match Apple San Francisco's look and metrics, with full
//! Vietnamese coverage). Apple's own SF Pro is proprietary and licensed
//! for Apple platforms only, so it cannot legally be bundled into a
//! Windows app; Inter is the correct legal equivalent and what we ship.
//! Two weights are bundled as base64 *text* (same trick as the app icon
//! in `assets/icon-256.b64`, because this repo's GitHub write route
//! cannot carry binary files):
//! - `assets/font-inter-regular.b64` — Regular (body text)
//! - `assets/font-inter-semibold.b64` — SemiBold (headings)
//!   Both are subsets (Latin + Vietnamese + common punctuation, 339 glyphs,
//!   ~37 KB each) made with `fontTools` (`pyftsubset`, kern+liga only,
//!   no hinting — egui rasterizes its own glyphs). Source: the official
//!   rsms/inter GitHub release (Inter 4.1), SIL OFL 1.1.
//!
//! Palette: Discord blurple accent (#5865F2 → #7B61FF, matching the app
//! icon) on Discord-like dark surfaces. No extra images, no animations,
//! so the app stays fast and light.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use egui::style::{Selection, WidgetVisuals};
use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, RichText, Stroke, Style,
    TextStyle,
};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// One full set of theme colors. The dark palette is the original
/// Discord-like look; the light palette is its counterpart. The
/// accent colors (blurple, white) are identical in both and stay
/// plain constants below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Window / central-panel background.
    pub bg: Color32,
    /// Left navigation sidebar background.
    pub sidebar_bg: Color32,
    /// Card / panel surface.
    pub card: Color32,
    /// Subtle card border.
    pub card_border: Color32,
    /// Default button background.
    pub btn_bg: Color32,
    /// Default button background on hover.
    pub btn_hover: Color32,
    /// Text-edit background.
    pub input_bg: Color32,
    /// Primary text.
    pub text: Color32,
    /// Secondary text.
    pub text_secondary: Color32,
    /// Muted / hint text.
    pub text_muted: Color32,
    /// Danger (Emergency Stop, delete).
    pub danger: Color32,
    /// Success (sent, running).
    pub success: Color32,
    /// Warning (status banner, caution text).
    pub warning: Color32,
}

/// Discord-like dark palette (the original look).
pub const DARK_PALETTE: Palette = Palette {
    bg: Color32::from_rgb(0x1E, 0x1F, 0x22),
    sidebar_bg: Color32::from_rgb(0x16, 0x17, 0x19),
    card: Color32::from_rgb(0x2B, 0x2D, 0x31),
    card_border: Color32::from_rgb(0x3F, 0x41, 0x47),
    btn_bg: Color32::from_rgb(0x35, 0x37, 0x3C),
    btn_hover: Color32::from_rgb(0x40, 0x42, 0x49),
    input_bg: Color32::from_rgb(0x11, 0x12, 0x14),
    text: Color32::from_rgb(0xF2, 0xF3, 0xF5),
    text_secondary: Color32::from_rgb(0xB5, 0xBA, 0xC1),
    text_muted: Color32::from_rgb(0x80, 0x84, 0x8E),
    danger: Color32::from_rgb(0xED, 0x42, 0x45),
    success: Color32::from_rgb(0x57, 0xF2, 0x87),
    warning: Color32::from_rgb(0xFA, 0xA8, 0x1A),
};

/// Light palette (Discord light surfaces, dark text). The status
/// colors are darkened so small text stays readable on white.
pub const LIGHT_PALETTE: Palette = Palette {
    bg: Color32::from_rgb(0xF2, 0xF3, 0xF5),
    sidebar_bg: Color32::from_rgb(0xE3, 0xE5, 0xE8),
    card: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    card_border: Color32::from_rgb(0xC9, 0xCD, 0xD2),
    btn_bg: Color32::from_rgb(0xE3, 0xE5, 0xE8),
    btn_hover: Color32::from_rgb(0xD4, 0xD7, 0xDC),
    input_bg: Color32::from_rgb(0xE3, 0xE5, 0xE8),
    text: Color32::from_rgb(0x06, 0x06, 0x07),
    text_secondary: Color32::from_rgb(0x4E, 0x50, 0x58),
    text_muted: Color32::from_rgb(0x6D, 0x6F, 0x78),
    danger: Color32::from_rgb(0xD9, 0x2D, 0x20),
    success: Color32::from_rgb(0x24, 0x80, 0x46),
    warning: Color32::from_rgb(0x9A, 0x6A, 0x00),
};

/// Discord blurple — primary accent (matches the app icon).
pub const ACCENT: Color32 = Color32::from_rgb(0x58, 0x65, 0xF2);
/// Blurple gradient end (matches the app icon).
pub const ACCENT_LIGHT: Color32 = Color32::from_rgb(0x7B, 0x61, 0xFF);

pub const WHITE: Color32 = Color32::WHITE;

/// The user's theme choice. Stored as a code string in config
/// ("dark"/"light"), mirroring the language code in `i18n`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
}

impl ThemeChoice {
    /// Unknown codes fall back to Dark, like unknown languages fall
    /// back to Vietnamese.
    pub fn from_code(code: &str) -> Self {
        match code.trim().to_ascii_lowercase().as_str() {
            "light" => Self::Light,
            _ => Self::Dark,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    pub fn palette(self) -> Palette {
        match self {
            Self::Dark => DARK_PALETTE,
            Self::Light => LIGHT_PALETTE,
        }
    }
}

/// UI text size. Only the text sizes scale; spacing and padding
/// stay fixed so layouts never break. Unknown codes fall back to
/// Medium, like unknown themes fall back to Dark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UiScale {
    Small,
    #[default]
    Medium,
    Large,
}

impl UiScale {
    pub fn from_code(code: &str) -> Self {
        match code.trim().to_ascii_lowercase().as_str() {
            "small" => Self::Small,
            "large" => Self::Large,
            _ => Self::Medium,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }

    /// Multiplier applied to every text size in `apply_visuals`.
    pub fn factor(self) -> f32 {
        match self {
            Self::Small => 0.875,
            Self::Medium => 1.0,
            Self::Large => 1.15,
        }
    }
}

/// The palette currently in effect for the whole process (one window,
/// one theme). Set by `apply_theme`; read by every helper and screen
/// through `palette()`. A lock poisoning falls back to the dark
/// palette rather than panicking the UI.
static CURRENT_PALETTE: std::sync::RwLock<Palette> = std::sync::RwLock::new(DARK_PALETTE);

/// The palette currently in effect.
pub fn palette() -> Palette {
    CURRENT_PALETTE.read().map(|p| *p).unwrap_or(DARK_PALETTE)
}

// ---------------------------------------------------------------------------
// Fonts
// ---------------------------------------------------------------------------

/// Font family name for Inter Regular (body text).
pub const FONT_REGULAR: &str = "Inter-Regular";
/// Font family name for Inter SemiBold (headings, emphasis).
pub const FONT_SEMIBOLD: &str = "Inter-SemiBold";

/// Decode one embedded font. Newlines in the wrapped base64 are stripped
/// first so decoding never depends on the wrapping style.
fn decode_font(b64: &str) -> Option<Vec<u8>> {
    let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    B64.decode(cleaned).ok()
}

/// Install the bundled fonts into `ctx`.
/// Returns `true` when both weights installed (so headings can use SemiBold);
/// on any failure the default egui fonts stay in place and the UI still
/// works — it just falls back to the stock look.
fn install_fonts(ctx: &egui::Context) -> bool {
    let regular = decode_font(include_str!("../assets/font-inter-regular.b64"));
    let semibold = decode_font(include_str!("../assets/font-inter-semibold.b64"));
    let (Ok(regular), Ok(semibold)) = (regular.ok_or(()), semibold.ok_or(())) else {
        eprintln!("theme: bundled font failed to decode, keeping default fonts");
        return false;
    };
    // TTF magic: 0x00 0x01 0x00 0x00 — reject garbage before handing it to egui.
    if regular.len() < 10_000
        || semibold.len() < 10_000
        || regular[0..4] != [0x00, 0x01, 0x00, 0x00]
        || semibold[0..4] != [0x00, 0x01, 0x00, 0x00]
    {
        eprintln!("theme: bundled font data looks invalid, keeping default fonts");
        return false;
    }
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        FONT_REGULAR.to_owned(),
        Arc::new(FontData::from_owned(regular)),
    );
    fonts.font_data.insert(
        FONT_SEMIBOLD.to_owned(),
        Arc::new(FontData::from_owned(semibold)),
    );
    // Inter first for proportional text (Vietnamese + Latin).
    if let Some(list) = fonts.families.get_mut(&FontFamily::Proportional) {
        list.insert(0, FONT_REGULAR.to_owned());
    }
    // Same for monospace (logs can contain Vietnamese scenario names).
    if let Some(list) = fonts.families.get_mut(&FontFamily::Monospace) {
        list.insert(0, FONT_REGULAR.to_owned());
    }
    // Named SemiBold family for headings / emphasis.
    fonts.families.insert(
        FontFamily::Name(FONT_SEMIBOLD.into()),
        vec![FONT_SEMIBOLD.to_owned(), FONT_REGULAR.to_owned()],
    );
    ctx.set_fonts(fonts);
    true
}

// ---------------------------------------------------------------------------
// Theme application
// ---------------------------------------------------------------------------

/// Install fonts + visuals + text styles + spacing on `ctx` for the
/// given theme. Called once at startup (from the eframe creation
/// closure) and again whenever the user switches theme in Settings
/// (font installation is idempotent, so re-running it is harmless).
pub fn apply_theme(ctx: &egui::Context, choice: ThemeChoice, scale: UiScale) {
    if let Ok(mut current) = CURRENT_PALETTE.write() {
        *current = choice.palette();
    }
    let semibold_ok = install_fonts(ctx);
    let mut style = (*ctx.style()).clone();
    apply_visuals(&mut style, semibold_ok, choice, scale.factor());
    ctx.set_style(style);
}

fn widget_visuals(bg: Color32, weak_bg: Color32, stroke: Stroke, fg: Color32) -> WidgetVisuals {
    WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: weak_bg,
        bg_stroke: stroke,
        corner_radius: CornerRadius::same(8),
        fg_stroke: Stroke::new(1.0_f32, fg),
        expansion: 0.0,
    }
}

fn apply_visuals(style: &mut Style, semibold_ok: bool, choice: ThemeChoice, scale: f32) {
    let p = choice.palette();
    let heading_family = if semibold_ok {
        FontFamily::Name(FONT_SEMIBOLD.into())
    } else {
        FontFamily::Proportional
    };

    // -- Text sizes (all multiplied by the UI scale) ------------------------
    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(24.0 * scale, heading_family.clone()),
    );
    style.text_styles.insert(
        TextStyle::Body,
        FontId::new(15.0 * scale, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Button,
        FontId::new(15.0 * scale, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Small,
        FontId::new(13.0 * scale, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(13.0 * scale, FontFamily::Monospace),
    );
    style.text_styles.insert(
        TextStyle::Name("SectionTitle".into()),
        FontId::new(17.0 * scale, heading_family.clone()),
    );
    style.text_styles.insert(
        TextStyle::Name("BigNumber".into()),
        FontId::new(28.0 * scale, heading_family),
    );

    // -- Spacing ------------------------------------------------------------
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    style.spacing.indent = 20.0;
    style.spacing.window_margin = egui::Margin::same(12);

    // -- Colors -------------------------------------------------------------
    let hover_stroke = if choice == ThemeChoice::Dark {
        Color32::from_rgb(0x4E, 0x50, 0x57)
    } else {
        Color32::from_rgb(0xB5, 0xB9, 0xC0)
    };
    let v = &mut style.visuals;
    v.dark_mode = choice == ThemeChoice::Dark;
    v.override_text_color = Some(p.text);
    v.window_fill = p.bg;
    v.panel_fill = p.bg;
    v.window_stroke = Stroke::new(1.0_f32, p.card_border);
    v.window_corner_radius = CornerRadius::same(8);
    v.faint_bg_color = p.card;
    v.extreme_bg_color = p.input_bg;
    v.code_bg_color = p.input_bg;
    v.warn_fg_color = p.warning;
    v.error_fg_color = p.danger;
    v.hyperlink_color = ACCENT_LIGHT;
    v.selection = Selection {
        bg_fill: ACCENT,
        stroke: Stroke::new(1.0_f32, WHITE),
    };
    v.widgets.noninteractive = widget_visuals(
        p.card,
        Color32::TRANSPARENT,
        Stroke::new(1.0_f32, p.card_border),
        p.text,
    );
    v.widgets.inactive = widget_visuals(
        p.btn_bg,
        p.btn_bg,
        Stroke::new(1.0_f32, p.card_border),
        p.text,
    );
    v.widgets.hovered = widget_visuals(
        p.btn_hover,
        p.btn_hover,
        Stroke::new(1.0_f32, hover_stroke),
        p.text,
    );
    v.widgets.active = widget_visuals(ACCENT, ACCENT, Stroke::new(1.0_f32, ACCENT), WHITE);
    v.widgets.open = widget_visuals(
        p.btn_hover,
        p.btn_hover,
        Stroke::new(1.0_f32, hover_stroke),
        p.text,
    );
}

// ---------------------------------------------------------------------------
// Layout helpers (shared by all screens)
// ---------------------------------------------------------------------------

/// Rounded card frame: theme surface, subtle border, generous padding.
pub fn card_frame() -> egui::Frame {
    let p = palette();
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0_f32, p.card_border))
        .corner_radius(10)
        .inner_margin(14)
}

/// Slim card frame (tighter padding) for the Builder action bar, where
/// every pixel of height counts in the 700 px window.
pub fn slim_card_frame() -> egui::Frame {
    let p = palette();
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0_f32, p.card_border))
        .corner_radius(10)
        .inner_margin(10)
}

/// Card with a red border (warnings, danger zone).
pub fn danger_frame() -> egui::Frame {
    let p = palette();
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0_f32, p.danger))
        .corner_radius(10)
        .inner_margin(14)
}

/// A full-width card. The inner `Ui` is widened first so the card spans
/// the available width instead of shrinking to its content.
pub fn card<R>(
    ui: &mut egui::Ui,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        add_contents(ui)
    })
}

/// Section title inside a card (SemiBold 17px).
pub fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).text_style(TextStyle::Name("SectionTitle".into())));
}

/// Small muted hint / secondary text.
pub fn muted(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).small().color(palette().text_muted));
}

/// SemiBold text at any size (headings, numbers, emphasis).
/// Use this instead of `RichText::strong()` — egui's strong rendering does
/// not know about the bundled SemiBold weight.
pub fn semibold(text: &str, size: f32) -> RichText {
    RichText::new(text).font(FontId::new(size, FontFamily::Name(FONT_SEMIBOLD.into())))
}

/// Primary action button (blurple fill, white text).
pub fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).color(WHITE)).fill(ACCENT))
}

/// Danger button (red fill, white text) — Emergency Stop, delete.
pub fn danger_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).color(WHITE)).fill(palette().danger))
}

/// Secondary button (transparent fill, subtle border).
pub fn ghost_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let p = palette();
    ui.add(
        egui::Button::new(RichText::new(text).color(p.text_secondary))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0_f32, p.card_border)),
    )
}

/// Full-width sidebar navigation button. Active item is blurple with white
/// SemiBold text; inactive items are transparent with secondary text.
pub fn nav_button(ui: &mut egui::Ui, label: &str, active: bool) -> egui::Response {
    let btn = if active {
        egui::Button::new(semibold(label, 15.0).color(WHITE)).fill(ACCENT)
    } else {
        egui::Button::new(RichText::new(label).color(palette().text_secondary))
            .fill(Color32::TRANSPARENT)
    };
    ui.add_sized([ui.available_width(), 36.0], btn)
}

/// Big-number stat card (value on top, muted label below).
pub fn stat_card(ui: &mut egui::Ui, label: &str, value: &str) {
    card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(value).text_style(TextStyle::Name("BigNumber".into())));
        ui.label(RichText::new(label).small().color(palette().text_muted));
    });
}

/// Colored bullet "•" (U+2022 is in the bundled font) for status dots.
pub fn dot(color: Color32) -> RichText {
    RichText::new("•").size(18.0).color(color)
}

/// Daily-cap progress bar, drawn by hand.
///
/// egui's stock `ProgressBar` always paints a minimum-width fill
/// (`min_width = 2 * corner_radius`), so at 0/300 it shows a small blue
/// pill. This one paints no fill at all at 0, a left-rounded fill while
/// partial, and a fully rounded fill when (nearly) complete.
pub fn progress_bar(ui: &mut egui::Ui, frac: f32, text: &str) {
    let frac = frac.clamp(0.0, 1.0);
    let height = 18.0;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
    let painter = ui.painter();
    let p = palette();
    // Track.
    painter.rect_filled(rect, CornerRadius::same(6), p.input_bg);
    painter.rect_stroke(
        rect,
        CornerRadius::same(6),
        Stroke::new(1.0_f32, p.card_border),
        egui::StrokeKind::Inside,
    );
    // Fill — nothing at 0, so no pill.
    if frac > 0.0 {
        let w = (rect.width() * frac).max(4.0);
        let fill = egui::Rect::from_min_size(rect.min, egui::vec2(w, height));
        let radius = if w >= rect.width() - 1.0 {
            CornerRadius::same(6)
        } else {
            CornerRadius {
                nw: 6,
                ne: 0,
                sw: 6,
                se: 0,
            }
        };
        painter.rect_filled(fill, radius, ACCENT);
    }
    // Centered label on top.
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        FontId::new(12.0, FontFamily::Proportional),
        p.text_secondary,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that call `apply_theme` share the process-wide palette
    /// (`CURRENT_PALETTE`), so they must not run concurrently: one
    /// test setting Dark while another asserts Light is a race.
    /// It failed reliably on CI's multi-core runners (run #57, all
    /// three OSes) while passing on a 1.5-CPU VM. Every test that
    /// calls `apply_theme` holds this lock for its whole body,
    /// including its restore step.
    static PALETTE_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_palette_for_test() -> std::sync::MutexGuard<'static, ()> {
        PALETTE_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn theme_choice_codes_and_palettes() {
        assert_eq!(ThemeChoice::from_code("dark"), ThemeChoice::Dark);
        assert_eq!(ThemeChoice::from_code("LIGHT"), ThemeChoice::Light);
        assert_eq!(ThemeChoice::from_code("bogus"), ThemeChoice::Dark);
        assert_eq!(ThemeChoice::from_code(""), ThemeChoice::Dark);
        assert_eq!(ThemeChoice::Dark.code(), "dark");
        assert_eq!(ThemeChoice::Light.code(), "light");
        assert_eq!(ThemeChoice::Dark.palette(), DARK_PALETTE);
        assert_eq!(ThemeChoice::Light.palette(), LIGHT_PALETTE);
        assert_ne!(DARK_PALETTE, LIGHT_PALETTE);
        // The light theme must actually be light, the dark one dark.
        assert!(LIGHT_PALETTE.bg.r() > 200);
        assert!(DARK_PALETTE.bg.r() < 60);
        assert!(LIGHT_PALETTE.text.r() < 60);
        assert!(DARK_PALETTE.text.r() > 200);
    }

    #[test]
    fn ui_scale_from_code_and_factors() {
        assert_eq!(UiScale::from_code("small"), UiScale::Small);
        assert_eq!(UiScale::from_code("LARGE"), UiScale::Large);
        assert_eq!(UiScale::from_code("bogus"), UiScale::Medium);
        assert_eq!(UiScale::from_code(""), UiScale::Medium);
        assert_eq!(UiScale::Small.code(), "small");
        assert_eq!(UiScale::Medium.code(), "medium");
        assert_eq!(UiScale::Large.code(), "large");
        assert!(UiScale::Small.factor() < 1.0);
        assert_eq!(UiScale::Medium.factor(), 1.0);
        assert!(UiScale::Large.factor() > 1.0);
    }

    #[test]
    fn apply_theme_scales_text_sizes() {
        let _guard = lock_palette_for_test();
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            apply_theme(ctx, ThemeChoice::Dark, UiScale::Large);
        });
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            let body = ctx.style().text_styles[&TextStyle::Body].size;
            assert!((body - 15.0 * UiScale::Large.factor()).abs() < f32::EPSILON);
        });
        // Restore medium so other tests see the default sizes.
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            apply_theme(ctx, ThemeChoice::Dark, UiScale::Medium);
        });
    }

    #[test]
    fn apply_theme_light_sets_light_visuals() {
        let _guard = lock_palette_for_test();
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            apply_theme(ctx, ThemeChoice::Light, UiScale::Medium);
        });
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            assert!(!ctx.style().visuals.dark_mode);
            assert_eq!(ctx.style().visuals.panel_fill, LIGHT_PALETTE.bg);
            assert_eq!(palette(), LIGHT_PALETTE);
        });
        // Restore dark so other tests (and the process) see the default.
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            apply_theme(ctx, ThemeChoice::Dark, UiScale::Medium);
        });
        assert_eq!(palette(), DARK_PALETTE);
    }

    #[test]
    fn embedded_fonts_decode_and_look_like_ttf() {
        for b64 in [
            include_str!("../assets/font-inter-regular.b64"),
            include_str!("../assets/font-inter-semibold.b64"),
        ] {
            let bytes = decode_font(b64).expect("font base64 must decode");
            assert!(bytes.len() > 10_000, "subset font unexpectedly small");
            assert_eq!(bytes[0..4], [0x00, 0x01, 0x00, 0x00], "not a TrueType font");
        }
    }

    #[test]
    fn apply_theme_installs_fonts_and_sizes() {
        let _guard = lock_palette_for_test();
        let ctx = egui::Context::default();
        // Pass 1: install the theme. Fonts set via `set_fonts` are
        // (re)loaded on the *next* pass, so glyph checks need pass 2.
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            apply_theme(ctx, ThemeChoice::Dark, UiScale::Medium);
        });
        // Pass 2: fonts are live now — assert sizes and glyph coverage.
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            // Heading must use the bundled SemiBold family at 24px.
            let heading = ctx.style().text_styles[&TextStyle::Heading].clone();
            assert_eq!(heading.size, 24.0);
            assert_eq!(
                heading.family,
                FontFamily::Name(FONT_SEMIBOLD.into()),
                "headings must use the bundled SemiBold font"
            );
            assert_eq!(ctx.style().text_styles[&TextStyle::Body].size, 15.0);
            // The installed fonts must really contain Vietnamese glyphs
            // (checked through egui's own font stack, not just the file bytes).
            let prop = FontId::new(15.0, FontFamily::Proportional);
            let semi = FontId::new(15.0, FontFamily::Name(FONT_SEMIBOLD.into()));
            for (font_id, ch) in [
                (&prop, 'ệ'),
                (&prop, 'Đ'),
                (&prop, 'ỗ'),
                (&prop, 'ư'),
                (&prop, '•'),
                (&semi, 'ệ'),
                (&semi, 'Đ'),
            ] {
                assert!(
                    ctx.fonts(|f| f.has_glyph(font_id, ch)),
                    "bundled font {font_id:?} is missing glyph {ch:?}"
                );
            }
        });
    }
}
