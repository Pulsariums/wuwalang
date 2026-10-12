use eframe::egui::{self, RichText, ViewportBuilder, ViewportId};

use super::state::Rect;
use super::{BG, FG};

const WIDTH: f32 = 240.0;
const HEIGHT: f32 = 26.0;
const MARGIN: f32 = 10.0;

pub fn show(ctx: &egui::Context, text: &str, game: Rect) {
    let ppp = ctx.pixels_per_point();
    let (_left, top, right, _bottom) = game;
    let x = right as f32 / ppp - WIDTH - MARGIN;
    let y = top as f32 / ppp + MARGIN;
    let builder = ViewportBuilder::default()
        .with_title("WuwaLang")
        .with_inner_size([WIDTH, HEIGHT])
        .with_position([x, y])
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top()
        .with_mouse_passthrough(true)
        .with_taskbar(false)
        .with_active(false)
        .with_resizable(false);
    ctx.show_viewport_immediate(
        ViewportId::from_hash_of("wuwalang-overlay"),
        builder,
        |ctx, _class| {
            egui::CentralPanel::default()
                .frame(egui::Frame::none().fill(BG))
                .show(ctx, |ui| {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new(text).color(FG).strong());
                    });
                });
        },
    );
}
