//! How the wallet looks: type sizes, spacing, an accent colour, cards, the QR code.
//! The light or dark theme follows the system; only the accent is ours.

use eframe::egui::{self, Color32, CornerRadius, FontId, Margin, RichText, Stroke, TextStyle};

/// The wallet's own colour, for the selected tab and the main action.
pub const ACCENT: Color32 = Color32::from_rgb(0x2f, 0x7d, 0xd8);
pub const INCOMING: Color32 = Color32::from_rgb(0x2e, 0x9e, 0x5b);
pub const OUTGOING: Color32 = Color32::from_rgb(0xc8, 0x4b, 0x3c);
pub const WARN: Color32 = Color32::from_rgb(0xd0, 0x80, 0x00);
pub const BAD: Color32 = Color32::from_rgb(0xd0, 0x30, 0x30);
pub const GOOD: Color32 = Color32::from_rgb(0x30, 0xa0, 0x50);

/// Content is laid out in a column no wider than this, centred.
pub const COLUMN: f32 = 760.0;

/// Sets type sizes, spacing and the accent on `ctx`. Called once, on the first frame.
pub fn apply(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        use egui::FontFamily::{Monospace, Proportional};
        style.text_styles = [
            (TextStyle::Heading, FontId::new(24.0, Proportional)),
            (TextStyle::Body, FontId::new(15.0, Proportional)),
            (TextStyle::Button, FontId::new(15.0, Proportional)),
            (TextStyle::Monospace, FontId::new(14.0, Monospace)),
            (TextStyle::Small, FontId::new(12.5, Proportional)),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(10.0, 9.0);
        style.spacing.button_padding = egui::vec2(14.0, 7.0);
        style.spacing.interact_size.y = 30.0;
        style.visuals.selection.bg_fill = ACCENT;
        style.visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        style.visuals.hyperlink_color = ACCENT;
        let r = CornerRadius::same(6);
        style.visuals.widgets.inactive.corner_radius = r;
        style.visuals.widgets.hovered.corner_radius = r;
        style.visuals.widgets.active.corner_radius = r;
        style.visuals.widgets.noninteractive.corner_radius = r;
    });
}

/// A bordered block with padding, for a group of related things.
pub fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::group(ui.style())
        .inner_margin(Margin::same(16))
        .corner_radius(CornerRadius::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// A small grey caption above a value.
pub fn caption(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).small().weak());
}

/// The main action on a screen: an accent-filled button.
pub fn primary(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(Color32::WHITE).strong())
            .fill(ACCENT)
            .min_size(egui::vec2(140.0, 34.0)),
    )
}

/// Lays `add` out in a centred column no wider than [`COLUMN`].
pub fn column<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let width = ui.available_width().min(COLUMN);
    let side = ((ui.available_width() - width) / 2.0).max(0.0);
    ui.horizontal(|ui| {
        ui.add_space(side);
        ui.vertical(|ui| {
            ui.set_width(width);
            add(ui)
        })
        .inner
    })
    .inner
}

/// Draws `text` as a QR code, `size` points square, dark modules on white with a
/// quiet zone, whatever the theme: phones read that best.
pub fn qr(ui: &mut egui::Ui, text: &str, size: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let Ok(code) = qrcode::QrCode::new(text.as_bytes()) else {
        return response;
    };
    let width = code.width();
    let colors = code.to_colors();
    let quiet = 2;
    let cells = (width + 2 * quiet) as f32;
    let cell = size / cells;
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::same(4), Color32::WHITE);
    for (i, c) in colors.iter().enumerate() {
        if *c != qrcode::Color::Dark {
            continue;
        }
        let x = (i % width + quiet) as f32;
        let y = (i / width + quiet) as f32;
        let min = rect.min + egui::vec2(x * cell, y * cell);
        painter.rect_filled(
            egui::Rect::from_min_size(min, egui::vec2(cell + 0.5, cell + 0.5)),
            CornerRadius::ZERO,
            Color32::BLACK,
        );
    }
    response
}
