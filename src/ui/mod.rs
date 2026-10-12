pub mod hotkey;
pub mod menu;
pub mod overlay;
pub mod state;

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use eframe::egui::{self, Color32};

use crate::config;
use hotkey::HotkeyEvent;
use menu::Menu;
use state::{Rect, Status};

pub const BG: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x2e);
pub const PANEL: Color32 = Color32::from_rgb(0x16, 0x21, 0x3e);
pub const ACCENT: Color32 = Color32::from_rgb(0x66, 0x7e, 0xea);
pub const ACCENT_END: Color32 = Color32::from_rgb(0x76, 0x4b, 0xa2);
pub const FG: Color32 = Color32::from_rgb(0xe6, 0xe6, 0xf0);
pub const MUTED: Color32 = Color32::from_rgb(0x6b, 0x6f, 0x9a);
pub const WARN: Color32 = Color32::from_rgb(0xff, 0x8a, 0x8a);
pub const OK: Color32 = Color32::from_rgb(0x7a, 0xd9, 0xa0);
pub const DANGER: Color32 = Color32::from_rgb(0xb5, 0x4a, 0x5f);

const REFRESH: Duration = Duration::from_secs(1);

fn initial_size() -> [f32; 2] {
    std::env::var("WUWALANG_SIZE")
        .ok()
        .and_then(|value| {
            let (width, height) = value.split_once('x')?;
            Some([width.parse().ok()?, height.parse().ok()?])
        })
        .unwrap_or([600.0, 760.0])
}

pub fn run() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("WuwaLang")
            .with_inner_size(initial_size())
            .with_min_inner_size([520.0, 600.0])
            .with_transparent(true),
        ..Default::default()
    };
    eframe::run_native(
        "WuwaLang",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
    .map_err(|error| anyhow!("Arayüz başlatılamadı: {error}"))
}

fn theme() -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = BG;
    visuals.extreme_bg_color = PANEL;
    visuals.faint_bg_color = PANEL;
    visuals.override_text_color = Some(FG);
    visuals.selection.bg_fill = ACCENT;
    visuals.window_rounding = egui::Rounding::same(8.0);
    for widget in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.noninteractive,
    ] {
        widget.bg_fill = PANEL;
        widget.weak_bg_fill = PANEL;
        widget.rounding = egui::Rounding::same(8.0);
    }
    for widget in [&mut visuals.widgets.hovered, &mut visuals.widgets.active] {
        widget.bg_fill = ACCENT_END;
        widget.weak_bg_fill = ACCENT_END;
        widget.rounding = egui::Rounding::same(8.0);
    }
    visuals
}

pub struct App {
    menu: Menu,
    hotkey: Receiver<HotkeyEvent>,
    config_path: PathBuf,
    status: Status,
    game_rect: Option<Rect>,
    last_refresh: Option<Instant>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        ctx.set_visuals(theme());
        let (tx, rx) = mpsc::channel();
        hotkey::spawn(tx, ctx.clone());
        let config_path = config::default_path();
        Self {
            menu: Menu::new(ctx, config_path.clone()),
            hotkey: rx,
            status: state::get_status(&config_path),
            config_path,
            game_rect: None,
            last_refresh: None,
        }
    }

    fn refresh(&mut self) {
        self.status = state::get_status(&self.config_path);
        self.game_rect = state::find_game_rect();
        self.last_refresh = Some(Instant::now());
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(event) = self.hotkey.try_recv() {
            match event {
                HotkeyEvent::Toggle => self.menu.toggle(ctx),
                HotkeyEvent::Failed(message) => self.menu.show_warning(ctx, message),
            }
        }
        if self.menu.take_refresh() || self.last_refresh.is_none_or(|at| at.elapsed() >= REFRESH) {
            self.refresh();
        }
        self.menu.ui(ctx, &self.status);
        if let (Some(text), Some(rect)) = (state::overlay_text(&self.status), self.game_rect) {
            overlay::show(ctx, &text, rect);
        }
        ctx.request_repaint_after(REFRESH);
    }
}
