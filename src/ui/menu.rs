use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{ensure, Result};
use eframe::egui::{self, Button, Color32, ProgressBar, RichText, TextEdit, ViewportCommand};
use serde_json::{json, Map, Value};

use super::state::{
    self, Action, Coverage, EditorFilter, Gate, GlossFilter, GlossRow, Package, Status,
};
use super::{ACCENT, BG, DANGER, MUTED, OK, PANEL, WARN};
use crate::backup::{self, Info};
use crate::batching::{self, Plan, Record, RunOptions, RunSummary};
use crate::config::{self, Config};
use crate::engines::{make_engine, Item, UreqClient};
use crate::game::{self, prepare_tools, Project, GAME_EXE};
use crate::glossary::Glossary;
use crate::lang::{self, Lang};
use crate::tm::{pull_pack, Entry as TmEntry, TranslationMemory};

type Logger = Arc<dyn Fn(&str) + Send + Sync>;
type ProgressFn = Arc<dyn Fn(f32, &str) + Send + Sync>;

const PAGE: usize = 50;
const SCROLL_GUTTER: f32 = 16.0;
const REMEASURE: Duration = Duration::from_secs(5);
const AMBER: Color32 = Color32::from_rgb(0xf5, 0xc2, 0x59);

const RISK_TEXT: &str =
    "Kuro Games'in Fair Play kuralları oyun verisinin değiştirilmesini yasaklar. \
Bu aracın kullanımında hesap kısıtlaması dahil tüm risk size aittir. Ana hesabınızla kullanmayın.";

const ENGINE_FIELDS: &[(&str, &[(&str, &str)])] = &[
    (
        "yandex",
        &[("api_key", "API anahtarı"), ("folder_id", "Folder ID")],
    ),
    ("google", &[("api_key", "API anahtarı")]),
    (
        "openai",
        &[
            ("api_key", "API anahtarı"),
            ("base_url", "Base URL"),
            ("model", "Model"),
        ],
    ),
    (
        "anthropic",
        &[
            ("api_key", "API anahtarı"),
            ("base_url", "Base URL"),
            ("model", "Model"),
        ],
    ),
];

fn engine_fields(engine: &str) -> &'static [(&'static str, &'static str)] {
    ENGINE_FIELDS
        .iter()
        .find(|(name, _)| *name == engine)
        .map(|(_, fields)| *fields)
        .unwrap_or(&[])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Status,
    Translate,
    Glossary,
    Editor,
    Backup,
    Packs,
}

#[derive(Debug, Clone)]
enum Job {
    Translate { dry: bool, limit: usize },
    Setup,
    Build,
    Uninstall,
    Kur,
    Rebuild,
    Toggle { enable: bool },
    ExportPack { dir: PathBuf },
    ImportPack { dir: PathBuf },
    PullPack { url: String },
    Backup,
    Restore { file: PathBuf },
}

impl Job {
    fn label(&self) -> &'static str {
        match self {
            Job::Translate { dry: true, .. } => "Tahmin",
            Job::Translate { dry: false, .. } => "Çeviri",
            Job::Setup => "Araç hazırlama",
            Job::Build => "Derleme",
            Job::Uninstall => "Kaldırma",
            Job::Kur => "Kurulum",
            Job::Rebuild => "Yeniden derleme ve kurulum",
            Job::Toggle { enable: true } => "Etkinleştirme",
            Job::Toggle { enable: false } => "Devre dışı bırakma",
            Job::ExportPack { .. } => "Paket dışa aktarma",
            Job::ImportPack { .. } => "Paket içe aktarma",
            Job::PullPack { .. } => "Repodan paket çekme",
            Job::Backup => "Yedek alma",
            Job::Restore { .. } => "Geri yükleme",
        }
    }
}

enum Msg {
    Log(String),
    Progress {
        fraction: f32,
        text: String,
    },
    Done {
        label: &'static str,
        result: Result<Option<RunSummary>, String>,
    },
    Coverage(Result<Coverage, String>),
    Notice(String),
}

#[derive(Clone)]
struct Reporter {
    log: Logger,
    progress: ProgressFn,
    cancel: Arc<AtomicBool>,
}

impl Reporter {
    fn step(&self, index: usize, total: usize, text: &str) {
        (self.progress)((index - 1) as f32 / total as f32, text);
        (self.log)(&format!("[{index}/{total}] {text}"));
    }
}

#[derive(Clone, Copy)]
enum GlossAction {
    Save,
    ToggleLock,
    AskDelete,
    CancelDelete,
    Delete,
}

pub struct Menu {
    ctx: egui::Context,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    config_path: PathBuf,
    lang: &'static Lang,
    tab: Tab,
    visible: bool,
    refresh_requested: bool,
    warning: Option<String>,
    engine: String,
    values: BTreeMap<String, String>,
    show_keys: bool,
    limit: String,
    batch_items: String,
    batch_chars: String,
    batch_threads: String,
    log: String,
    busy: bool,
    job_label: &'static str,
    cancelling: bool,
    cancel: Arc<AtomicBool>,
    progress: Option<(f32, String)>,
    run_progress: Option<(usize, usize)>,
    run_started: Option<Instant>,
    usage: Option<BTreeMap<String, u64>>,
    test_msg: String,
    coverage: Option<Coverage>,
    coverage_error: Option<String>,
    coverage_busy: bool,
    last_measure: Option<Instant>,
    glossary: Vec<GlossRow>,
    g_query: String,
    g_filter: GlossFilter,
    g_term: String,
    g_tr: String,
    g_lock_new: bool,
    g_msg: String,
    g_confirm_delete: Option<String>,
    g_clean: Option<Vec<String>>,
    editor: Vec<TmEntry>,
    e_query: String,
    e_filter: EditorFilter,
    e_page: usize,
    e_key: Option<(String, EditorFilter, usize)>,
    e_has_next: bool,
    e_buffers: BTreeMap<String, String>,
    e_msg: String,
    e_pending: Option<String>,
    backups: Vec<Info>,
    b_confirm: Option<PathBuf>,
    b_msg: String,
    game_exe: String,
    game_path: String,
    pack_url: String,
    accepted_risk: bool,
}

impl Menu {
    pub fn new(ctx: egui::Context, config_path: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        let (cfg, warning) = match config::load(&config_path, &std::env::vars().collect()) {
            Ok(cfg) => (cfg, None),
            Err(error) => (
                Config::default(),
                Some(format!("config.json okunamadı: {error:#}")),
            ),
        };
        let game_exe = if cfg.game_exe.trim().is_empty() {
            game::find_game(Some(Path::new(&cfg.game_path)))
                .map(|dir| dir.join(GAME_EXE).to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            cfg.game_exe.trim().to_owned()
        };
        let mut menu = Self {
            game_exe,
            game_path: cfg.game_path.clone(),
            pack_url: cfg.pack_url.clone(),
            accepted_risk: cfg.accepted_risk,
            lang: state::active_lang(&cfg),
            cancel: Arc::new(AtomicBool::new(false)),
            ctx,
            tx,
            rx,
            config_path,
            tab: initial_tab(),
            visible: true,
            refresh_requested: false,
            warning,
            engine: cfg.engine.clone(),
            values: BTreeMap::new(),
            show_keys: false,
            limit: "0".into(),
            batch_items: cfg.batch.max_items.to_string(),
            batch_chars: cfg.batch.max_chars.to_string(),
            batch_threads: cfg.batch.threads.to_string(),
            log: String::new(),
            busy: false,
            job_label: "",
            cancelling: false,
            progress: None,
            run_progress: None,
            run_started: None,
            usage: None,
            test_msg: String::new(),
            coverage: None,
            coverage_error: None,
            coverage_busy: false,
            last_measure: None,
            glossary: Vec::new(),
            g_query: String::new(),
            g_filter: GlossFilter::All,
            g_term: String::new(),
            g_tr: String::new(),
            g_lock_new: false,
            g_msg: String::new(),
            g_confirm_delete: None,
            g_clean: None,
            editor: Vec::new(),
            e_query: String::new(),
            e_filter: EditorFilter::All,
            e_page: 0,
            e_key: None,
            e_has_next: false,
            e_buffers: BTreeMap::new(),
            e_msg: String::new(),
            e_pending: None,
            backups: Vec::new(),
            b_confirm: None,
            b_msg: String::new(),
        };
        menu.load_values(&cfg);
        menu.reload_glossary();
        menu.reload_editor();
        menu.reload_backups();
        menu.spawn_measure();
        menu
    }

    fn load_values(&mut self, cfg: &Config) {
        let json = serde_json::to_value(cfg).unwrap_or(Value::Null);
        for (engine, fields) in ENGINE_FIELDS {
            for (key, _) in *fields {
                let value = json[engine][key].as_str().unwrap_or_default().to_owned();
                self.values.insert(format!("{engine}.{key}"), value);
            }
        }
    }

    pub fn toggle(&mut self, ctx: &egui::Context) {
        self.visible = !self.visible;
        ctx.send_viewport_cmd(ViewportCommand::Visible(self.visible));
        if self.visible {
            ctx.send_viewport_cmd(ViewportCommand::Focus);
        }
    }

    pub fn show_warning(&mut self, ctx: &egui::Context, message: String) {
        self.visible = true;
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        self.warning = Some(message);
    }

    pub fn take_refresh(&mut self) -> bool {
        std::mem::take(&mut self.refresh_requested)
    }

    fn pump(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Log(text) => {
                    for line in text.lines() {
                        if let Some((done, total)) = state::parse_batch_progress(line) {
                            self.on_batch(done, total);
                        }
                    }
                    self.log.push_str(&text);
                    if self.log.len() > 200_000 {
                        let mut cut = self.log.len() - 100_000;
                        while !self.log.is_char_boundary(cut) {
                            cut += 1;
                        }
                        self.log.drain(..cut);
                    }
                }
                Msg::Progress { fraction, text } => self.progress = Some((fraction, text)),
                Msg::Done { label, result } => {
                    self.busy = false;
                    self.cancelling = false;
                    self.progress = None;
                    self.run_progress = None;
                    self.run_started = None;
                    self.log.push_str(&format!("--- {label} bitti ---\n"));
                    match result {
                        Ok(Some(summary)) => {
                            self.log.push_str(&format!(
                                "{} çevrildi, {} başarısız, {} yeni terim.\n",
                                summary.translated, summary.failed, summary.terms_added
                            ));
                            self.usage = Some(summary.usage);
                        }
                        Ok(None) => {}
                        Err(error) if error.contains("iptal") => {
                            self.log
                                .push_str("Durduruldu. Biten kısımlar kayıtlıdır.\n");
                        }
                        Err(error) => {
                            self.log.push_str(&format!("Hata: {error}\n"));
                            self.warning = Some(error);
                        }
                    }
                    self.reload_glossary();
                    self.reload_editor();
                    self.reload_backups();
                    self.spawn_measure();
                    self.refresh_requested = true;
                }
                Msg::Coverage(result) => {
                    self.coverage_busy = false;
                    match result {
                        Ok(coverage) => {
                            self.coverage = Some(coverage);
                            self.coverage_error = None;
                        }
                        Err(error) => {
                            self.coverage = None;
                            self.coverage_error = Some(error);
                        }
                    }
                }
                Msg::Notice(text) => self.test_msg = text,
            }
        }
    }

    /// Çeviri partisi bittikçe ilerlemeyi günceller; yüzdeyi en fazla beş saniyede bir yeniden ölçer.
    fn on_batch(&mut self, done: usize, total: usize) {
        self.run_progress = Some((done, total));
        self.progress = Some((
            state::fraction(done, total),
            format!("{done} / {total} parti"),
        ));
        if self
            .last_measure
            .is_none_or(|at| at.elapsed() >= REMEASURE)
        {
            self.spawn_measure();
        }
    }

    fn spawn_measure(&mut self) {
        if self.coverage_busy {
            return;
        }
        self.coverage_busy = true;
        self.last_measure = Some(Instant::now());
        let lang = self.lang;
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        thread::spawn(move || {
            let result = state::measure(lang).map_err(|error| format!("{error:#}"));
            let _ = tx.send(Msg::Coverage(result));
            ctx.request_repaint();
        });
    }

    fn set_lang(&mut self, code: &str) {
        match config::save(&json!({ "target": code }), &self.config_path) {
            Ok(()) => {
                if let Ok(lang) = lang::validate(code) {
                    self.lang = lang;
                }
                self.coverage = None;
                self.coverage_error = None;
                self.reload_glossary();
                self.reload_editor();
                self.reload_backups();
                self.spawn_measure();
                self.refresh_requested = true;
            }
            Err(error) => self.warning = Some(format!("Kaydedilemedi: {error:#}")),
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, status: &Status) {
        self.pump();
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| self.status_bar(ui, status));
        egui::TopBottomPanel::bottom("log")
            .resizable(true)
            .default_height(170.0)
            .show(ctx, |ui| self.log_view(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(BG).inner_margin(12.0))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_max_width(ui.available_width() - SCROLL_GUTTER);
                        self.header(ui);
                        self.setup_section(ui, status);
                        ui.add_space(4.0);
                        ui.separator();
                        if let Some(warning) = &self.warning {
                            ui.colored_label(WARN, warning.as_str());
                        }
                        ui.horizontal_wrapped(|ui| {
                            ui.selectable_value(&mut self.tab, Tab::Status, "Durum");
                            ui.selectable_value(&mut self.tab, Tab::Translate, "Çeviri");
                            ui.selectable_value(&mut self.tab, Tab::Glossary, "Sözlük");
                            ui.selectable_value(&mut self.tab, Tab::Editor, "Düzenleyici");
                            ui.selectable_value(&mut self.tab, Tab::Backup, "Yedek");
                            ui.selectable_value(&mut self.tab, Tab::Packs, "Paketler");
                        });
                        ui.separator();
                        match self.tab {
                            Tab::Status => self.status_tab(ctx, ui, status),
                            Tab::Translate => self.translate_tab(ui),
                            Tab::Glossary => self.glossary_tab(ui),
                            Tab::Editor => self.editor_tab(ui),
                            Tab::Backup => self.backup_tab(ui),
                            Tab::Packs => self.packs_tab(ui),
                        }
                    });
            });
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("WuwaLang").size(22.0).strong().color(ACCENT));
            ui.label(
                RichText::new(format!(
                    "Wuthering Waves · {} ({})",
                    self.lang.native, self.lang.code
                ))
                .color(MUTED),
            );
        });
        let fraction = self.coverage.map_or(0.0, |c| (c.percent / 100.0) as f32);
        let text = state::coverage_text(self.coverage.as_ref(), self.coverage_error.as_deref());
        ui.add(
            ProgressBar::new(fraction)
                .text(text)
                .animate(self.coverage_busy)
                .desired_width(ui.available_width()),
        );
        ui.add_space(6.0);
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, status: &Status) {
        ui.horizontal_wrapped(|ui| {
            if self.busy {
                ui.spinner();
                ui.label(format!("{} çalışıyor", self.job_label));
                if let Some((fraction, text)) = &self.progress {
                    ui.add(
                        ProgressBar::new(*fraction)
                            .text(text.as_str())
                            .desired_width((ui.available_width() - 170.0).clamp(60.0, 240.0)),
                    );
                }
                if ui
                    .add_enabled(!self.cancelling, Button::new("Durdur"))
                    .clicked()
                {
                    self.request_cancel();
                }
                if self.cancelling {
                    ui.colored_label(WARN, "Durduruluyor…");
                }
            } else {
                ui.label(RichText::new("Hazır").color(MUTED));
            }
            let game = if status.game_running {
                "Oyun açık"
            } else {
                "Oyun kapalı"
            };
            ui.label(RichText::new(game).color(MUTED));
        });
    }

    fn setup_section(&mut self, ui: &mut egui::Ui, status: &Status) {
        if !self.accepted_risk {
            self.risk_section(ui);
            ui.add_space(6.0);
        }
        ui.label(RichText::new("Oyun yolu").strong());
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 100.0).max(120.0);
            ui.add(
                TextEdit::singleline(&mut self.game_exe)
                    .hint_text("...\\Wuthering Waves Game\\Wuthering Waves.exe")
                    .desired_width(width),
            );
            if ui.add_enabled(!self.busy, Button::new("Gözat")).clicked() {
                self.browse();
            }
        });
        ui.horizontal(|ui| {
            ui.label("Hedef dil:");
            let mut chosen: Option<&str> = None;
            egui::ComboBox::from_id_salt("target_lang")
                .selected_text(format!("{} ({})", self.lang.native, self.lang.code))
                .show_ui(ui, |ui| {
                    for option in lang::LANGS {
                        let label = format!("{} ({})", option.native, option.code);
                        if ui.selectable_label(option.code == self.lang.code, label).clicked()
                        {
                            chosen = Some(option.code);
                        }
                    }
                });
            if let Some(code) = chosen {
                self.set_lang(code);
            }
        });
        let root = resolve_root(&self.game_exe, &self.game_path);
        match &root {
            Ok(_) => {
                ui.label(format!(
                    "Geçerli · Kurulu: {} · Oyun açık: {}",
                    yes_no(status.installed),
                    yes_no(status.game_running)
                ));
            }
            Err(error) => {
                ui.colored_label(WARN, format!("Bulunamadı: {error:#}"));
            }
        }
        let gate = self.gate(status, root.is_ok());
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            if action_button(ui, "Kur", Action::Install, gate, Style::Primary) {
                self.start_job(Job::Kur);
            }
            if action_button(ui, "Kaldır", Action::Remove, gate, Style::Danger) {
                self.start_job(Job::Uninstall);
            }
            let toggle = state::toggle_label(status.enabled);
            if action_button(ui, toggle, Action::Toggle, gate, Style::Plain) {
                self.start_job(Job::Toggle {
                    enable: !status.enabled,
                });
            }
            if action_button(
                ui,
                "Yeniden derle ve kur",
                Action::Rebuild,
                gate,
                Style::Plain,
            ) {
                self.start_job(Job::Rebuild);
            }
        });
        if status.game_running {
            ui.colored_label(WARN, "Oyun açık. Kur, kaldırma ve etkinleştirme için önce oyunu kapat.");
        } else if !self.accepted_risk {
            ui.colored_label(MUTED, "Kur için yukarıdaki Fair Play onayını işaretle.");
        }
    }

    fn gate(&self, status: &Status, root_ok: bool) -> Gate {
        Gate {
            busy: self.busy,
            game_running: status.game_running,
            root_ok,
            installed: status.installed,
            accepted_risk: self.accepted_risk,
        }
    }

    fn risk_section(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style())
            .fill(PANEL)
            .rounding(8.0)
            .inner_margin(10.0)
            .show(ui, |ui| {
                ui.colored_label(WARN, "Fair Play uyarısı (tek seferlik)");
                ui.label(RISK_TEXT);
                let mut accepted = self.accepted_risk;
                if ui
                    .checkbox(&mut accepted, "Riski anladım ve kabul ediyorum")
                    .changed()
                    && accepted
                {
                    self.accept_risk();
                }
            });
    }

    fn accept_risk(&mut self) {
        match config::save(&json!({ "accepted_risk": true }), &self.config_path) {
            Ok(()) => self.accepted_risk = true,
            Err(error) => self.warning = Some(format!("Kaydedilemedi: {error:#}")),
        }
    }

    fn browse(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Wuthering Waves exe seç")
            .add_filter("Oyun exe", &["exe"])
            .pick_file()
        else {
            return;
        };
        match game::root_from_exe(&path) {
            Ok(_) => {
                self.game_exe = path.to_string_lossy().into_owned();
                self.warning = None;
                self.save_game_exe();
            }
            Err(error) => self.warning = Some(format!("{error:#}")),
        }
    }

    fn save_game_exe(&mut self) -> bool {
        let exe = self.game_exe.trim().to_owned();
        match config::save(&json!({ "game_exe": exe.as_str() }), &self.config_path) {
            Ok(()) => {
                self.game_exe = exe;
                true
            }
            Err(error) => {
                self.warning = Some(format!("Kaydedilemedi: {error:#}"));
                false
            }
        }
    }

    fn status_tab(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, status: &Status) {
        let percent = self
            .coverage
            .map_or("—".to_string(), |c| format!("%{:.1}", c.percent).replace('.', ","));
        let game_text = if status.game_running { "Açık" } else { "Kapalı" };
        let game_color = if status.game_running { WARN } else { OK };
        let engine = status
            .engine
            .clone()
            .unwrap_or_else(|| "okunamadı".to_string());
        let lang_text = format!("{} ({})", self.lang.native, self.lang.code);
        let cards = [
            ("Oyun", game_text.to_string(), Some(game_color)),
            (
                "Paket",
                status.package().label().to_string(),
                Some(package_color(status.package())),
            ),
            ("Çeviri", percent, None),
            ("TM kaydı", status.tm_count.to_string(), None),
            ("Sözlük terimi", status.glossary_count.to_string(), None),
            ("Hedef dil", lang_text, None),
            ("Motor", engine, None),
        ];
        card_grid(ui, &cards);
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button("Yenile").clicked() {
                self.refresh_requested = true;
                self.spawn_measure();
            }
            if ui.button("Çıkış").clicked() {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            if ui
                .add_enabled(!self.busy, Button::new("Araçları hazırla"))
                .clicked()
            {
                self.start_job(Job::Setup);
            }
        });
    }

    fn translate_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Motor:");
            egui::ComboBox::from_id_salt("engine")
                .selected_text(self.engine.clone())
                .show_ui(ui, |ui| {
                    for name in config::ENGINES {
                        ui.selectable_value(&mut self.engine, name.to_owned(), name);
                    }
                });
            ui.label("Limit:");
            ui.add(TextEdit::singleline(&mut self.limit).desired_width(70.0));
            ui.checkbox(&mut self.show_keys, "Anahtarı göster");
        });
        ui.add_space(6.0);
        let engine = self.engine.clone();
        for (key, title) in engine_fields(&engine) {
            ui.horizontal(|ui| {
                ui.add_sized([110.0, 20.0], egui::Label::new(format!("{title}:")));
                let value = self.values.entry(format!("{engine}.{key}")).or_default();
                ui.add(
                    TextEdit::singleline(value)
                        .password(*key == "api_key" && !self.show_keys)
                        .desired_width(ui.available_width()),
                );
            });
        }
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.label("Parti öğe:");
            ui.add(TextEdit::singleline(&mut self.batch_items).desired_width(60.0));
            ui.label("Parti karakter:");
            ui.add(TextEdit::singleline(&mut self.batch_chars).desired_width(70.0));
            ui.label("Thread:");
            ui.add(TextEdit::singleline(&mut self.batch_threads).desired_width(50.0));
        });
        ui.add_space(6.0);
        let idle = !self.busy;
        ui.horizontal_wrapped(|ui| {
            if ui.button("Kaydet").clicked() {
                self.save_engine();
            }
            if ui
                .add_enabled(idle, Button::new("Bağlantıyı dene"))
                .clicked()
            {
                self.test_connection();
            }
            if ui
                .add_enabled(idle, Button::new("Tahmin (dry-run)"))
                .clicked()
            {
                self.start_translate(true);
            }
            if ui
                .add_enabled(idle, Button::new("Çevir").fill(ACCENT))
                .clicked()
            {
                self.start_translate(false);
            }
            if ui
                .add_enabled(!idle && !self.cancelling, Button::new("Durdur"))
                .clicked()
            {
                self.request_cancel();
            }
        });
        if self.cancelling {
            ui.colored_label(WARN, "Durduruluyor… Biten kısımlar kayıtlıdır.");
        }
        if !self.test_msg.is_empty() {
            ui.label(self.test_msg.as_str());
        }
        if let (Some(started), Some((done, total))) = (self.run_started, self.run_progress) {
            let elapsed = started.elapsed();
            let remaining = state::eta(elapsed, done, total)
                .map_or("hesaplanıyor".to_string(), state::format_duration);
            ui.label(format!(
                "Geçen: {} · Kalan: {} · {} / {} parti",
                state::format_duration(elapsed),
                remaining,
                done,
                total
            ));
        }
        let usage = self
            .usage
            .as_ref()
            .map_or("Kullanım bilgisi yok".to_string(), state::usage_text);
        ui.label(RichText::new(format!("Kullanım: {usage}")).color(MUTED));
    }

    fn glossary_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.add(
                TextEdit::singleline(&mut self.g_query)
                    .hint_text("Ara (terim ya da karşılık)")
                    .desired_width(220.0),
            );
            egui::ComboBox::from_id_salt("gloss_filter")
                .selected_text(self.g_filter.label())
                .show_ui(ui, |ui| {
                    for filter in GlossFilter::ALL {
                        ui.selectable_value(&mut self.g_filter, filter, filter.label());
                    }
                });
            let shown = state::filter_glossary(&self.glossary, &self.g_query, self.g_filter);
            ui.label(format!("{} / {}", shown.len(), self.glossary.len()));
        });
        let shown = state::filter_glossary(&self.glossary, &self.g_query, self.g_filter);
        let confirming = self.g_confirm_delete.clone();
        let mut action: Option<(usize, GlossAction)> = None;
        egui::ScrollArea::vertical()
            .id_salt("gloss_list")
            .max_height(280.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for &index in &shown {
                    let row = &mut self.glossary[index];
                    egui::Frame::group(ui.style())
                        .fill(PANEL)
                        .rounding(6.0)
                        .inner_margin(6.0)
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new(row.term.as_str()).strong());
                                if row.locked {
                                    ui.colored_label(WARN, "[kilitli]");
                                }
                                if row.auto {
                                    ui.colored_label(MUTED, "[otomatik]");
                                }
                            });
                            ui.add(
                                TextEdit::multiline(&mut row.tr)
                                    .desired_rows(1)
                                    .desired_width(ui.available_width()),
                            );
                            ui.horizontal_wrapped(|ui| {
                                if row.tr != row.saved_tr && ui.button("Kaydet").clicked() {
                                    action = Some((index, GlossAction::Save));
                                }
                                let lock_label = if row.locked { "Kilidi aç" } else { "Kilitle" };
                                if ui.button(lock_label).clicked() {
                                    action = Some((index, GlossAction::ToggleLock));
                                }
                                if confirming.as_deref() == Some(row.term.as_str()) {
                                    ui.colored_label(WARN, "Silinsin mi?");
                                    if ui.button("Onayla").clicked() {
                                        action = Some((index, GlossAction::Delete));
                                    }
                                    if ui.button("Vazgeç").clicked() {
                                        action = Some((index, GlossAction::CancelDelete));
                                    }
                                } else if ui.button("Sil").clicked() {
                                    action = Some((index, GlossAction::AskDelete));
                                }
                            });
                        });
                }
            });
        if let Some((index, act)) = action {
            self.apply_gloss_action(index, act);
        }
        ui.separator();
        ui.label(RichText::new("Yeni terim").strong());
        ui.horizontal_wrapped(|ui| {
            ui.add(
                TextEdit::singleline(&mut self.g_term)
                    .hint_text("Terim")
                    .desired_width(160.0),
            );
            ui.add(
                TextEdit::singleline(&mut self.g_tr)
                    .hint_text("Karşılık")
                    .desired_width(160.0),
            );
            ui.checkbox(&mut self.g_lock_new, "Kilitli");
            if ui.button("Ekle").clicked() {
                self.glossary_add();
            }
        });
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button("Sözlüğü temizle").clicked() {
                self.glossary_clean_preview();
            }
        });
        if let Some(removed) = self.g_clean.clone() {
            let shown: Vec<&str> = removed.iter().take(30).map(String::as_str).collect();
            ui.colored_label(
                WARN,
                format!(
                    "{} kayıt silinecek (geçersiz ve kilitsiz): {}{}",
                    removed.len(),
                    shown.join(", "),
                    if removed.len() > 30 { " …" } else { "" }
                ),
            );
            ui.horizontal(|ui| {
                if ui.button("Onayla ve temizle").clicked() {
                    self.glossary_clean_confirm();
                }
                if ui.button("Vazgeç").clicked() {
                    self.g_clean = None;
                }
            });
        }
        ui.colored_label(MUTED, self.g_msg.as_str());
    }

    fn editor_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.add(
                TextEdit::singleline(&mut self.e_query)
                    .hint_text("Kaynak ya da çeviride ara")
                    .desired_width(220.0),
            );
            egui::ComboBox::from_id_salt("editor_filter")
                .selected_text(self.e_filter.label())
                .show_ui(ui, |ui| {
                    for filter in EditorFilter::ALL {
                        ui.selectable_value(&mut self.e_filter, filter, filter.label());
                    }
                });
        });
        ui.colored_label(
            MUTED,
            "Düzeltme metnin oyunda geçtiği her yerde değişir; oyuna yansıması için Yeniden derle ve kur.",
        );
        let key = (self.e_query.trim().to_owned(), self.e_filter, self.e_page);
        if self.e_key.as_ref() != Some(&key) {
            self.reload_editor();
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.e_page > 0, Button::new("Önceki"))
                .clicked()
            {
                self.e_page -= 1;
            }
            ui.label(format!("Sayfa {}", self.e_page + 1));
            if ui
                .add_enabled(self.e_has_next, Button::new("Sonraki"))
                .clicked()
            {
                self.e_page += 1;
            }
        });
        let rows = self.editor.clone();
        let mut save: Option<usize> = None;
        let mut reset: Option<String> = None;
        egui::ScrollArea::vertical()
            .id_salt("editor_list")
            .max_height(360.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (index, entry) in rows.iter().enumerate() {
                    egui::Frame::group(ui.style())
                        .fill(PANEL)
                        .rounding(6.0)
                        .inner_margin(6.0)
                        .show(ui, |ui| {
                            ui.label(RichText::new(entry.source.as_str()).strong());
                            let buffer = self
                                .e_buffers
                                .entry(entry.source.clone())
                                .or_insert_with(|| entry.translation.clone());
                            ui.add(
                                TextEdit::multiline(&mut *buffer)
                                    .desired_rows(2)
                                    .desired_width(ui.available_width()),
                            );
                            ui.horizontal_wrapped(|ui| {
                                if ui.button("Kaydet").clicked() {
                                    save = Some(index);
                                }
                                if ui.button("Sıfırla (yeniden çevrilsin)").clicked() {
                                    reset = Some(entry.source.clone());
                                }
                                if buffer.trim() != entry.translation {
                                    ui.colored_label(WARN, "kaydedilmemiş değişiklik");
                                }
                                if entry.engine == crate::tm::MANUAL_ENGINE {
                                    ui.colored_label(MUTED, "elle düzeltildi");
                                }
                            });
                        });
                }
            });
        if rows.is_empty() {
            ui.colored_label(MUTED, "Kayıt yok.");
        }
        if let Some(index) = save {
            let source = rows[index].source.clone();
            let text = self.e_buffers.get(&source).cloned().unwrap_or_default();
            self.save_manual(source, text);
        }
        if let Some(source) = reset {
            self.reset_entry(source);
        }
        ui.colored_label(MUTED, self.e_msg.as_str());
    }

    fn backup_tab(&mut self, ui: &mut egui::Ui) {
        let idle = !self.busy;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(idle, Button::new("Şimdi yedekle").fill(ACCENT))
                .clicked()
            {
                self.start_job(Job::Backup);
            }
            if ui.button("Yenile").clicked() {
                self.reload_backups();
            }
        });
        ui.colored_label(
            MUTED,
            "Yedekler backups klasöründe tutulur; dil başına en fazla 10 yedek saklanır. \
Otomatik yedek çeviri sonunda ve Kur/Kaldır/Derle öncesinde alınır.",
        );
        if let Some(path) = self.b_confirm.clone() {
            ui.colored_label(
                WARN,
                "Bu yedek geri yüklensin mi? Mevcut çeviri belleği önce yedeklenir.",
            );
            ui.horizontal(|ui| {
                if ui.add_enabled(idle, Button::new("Onayla")).clicked() {
                    self.b_confirm = None;
                    self.start_job(Job::Restore { file: path });
                }
                if ui.button("Vazgeç").clicked() {
                    self.b_confirm = None;
                }
            });
        }
        ui.add_space(4.0);
        if self.backups.is_empty() {
            ui.label("Henüz yedek yok.");
        }
        let backups = self.backups.clone();
        egui::ScrollArea::vertical()
            .id_salt("backup_list")
            .max_height(300.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for info in &backups {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(backup::display_stamp(&info.stamp)).strong());
                        let records = info.records.map_or("?".to_string(), |n| n.to_string());
                        ui.label(format!("{records} kayıt"));
                        ui.label(backup::format_bytes(info.bytes));
                        if ui
                            .add_enabled(idle, Button::new("Geri yükle"))
                            .clicked()
                        {
                            self.b_confirm = Some(info.path.clone());
                        }
                    });
                }
            });
        ui.colored_label(MUTED, self.b_msg.as_str());
    }

    fn packs_tab(&mut self, ui: &mut egui::Ui) {
        let enabled = !self.busy;
        ui.label(format!(
            "Topluluk paketi: {} altındaki *.jsonl dosyaları ve index.json.",
            state::pack_dir(self.lang).display()
        ));
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(enabled, Button::new("Dışa aktar")).clicked() {
                self.export_pack_dialog();
            }
            if ui.add_enabled(enabled, Button::new("İçe aktar")).clicked() {
                self.import_pack_dialog();
            }
            if ui.add_enabled(enabled, Button::new("Derle")).clicked() {
                self.start_job(Job::Build);
            }
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_sized([110.0, 20.0], egui::Label::new("Paket URL:"));
            ui.add(
                TextEdit::singleline(&mut self.pack_url)
                    .hint_text("https://.../packs/{lang}")
                    .desired_width(ui.available_width()),
            );
        });
        ui.add_space(6.0);
        if ui
            .add_enabled(enabled, Button::new("Repodan çek"))
            .clicked()
        {
            self.pull_pack_request();
        }
    }

    fn export_pack_dialog(&mut self) {
        let Some(dir) = rfd::FileDialog::new()
            .set_title("Paket dışa aktarma klasörü")
            .set_directory(state::pack_dir(self.lang))
            .pick_folder()
        else {
            return;
        };
        self.start_job(Job::ExportPack { dir });
    }

    fn import_pack_dialog(&mut self) {
        let Some(dir) = rfd::FileDialog::new()
            .set_title("İçe aktarılacak paket klasörü")
            .set_directory(state::pack_dir(self.lang))
            .pick_folder()
        else {
            return;
        };
        self.start_job(Job::ImportPack { dir });
    }

    fn pull_pack_request(&mut self) {
        let url = self.pack_url.trim().to_owned();
        if url.is_empty() {
            self.warning = Some("Paket URL'i boş.".into());
            return;
        }
        if let Err(error) = config::save(&json!({ "pack_url": url.as_str() }), &self.config_path) {
            self.warning = Some(format!("Kaydedilemedi: {error:#}"));
            return;
        }
        self.pack_url = url.clone();
        self.start_job(Job::PullPack { url });
    }

    fn log_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Günlük").strong());
            if ui.button("Temizle").clicked() {
                self.log.clear();
            }
        });
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                ui.label(RichText::new(&self.log).monospace());
            });
    }

    fn save_engine(&mut self) -> bool {
        let engine = self.engine.clone();
        let mut section = Map::new();
        for (key, _) in engine_fields(&engine) {
            let value = self
                .values
                .get(&format!("{engine}.{key}"))
                .cloned()
                .unwrap_or_default();
            section.insert((*key).to_owned(), Value::String(value.trim().to_owned()));
        }
        let parse = |text: &str| text.trim().parse::<usize>().ok().filter(|value| *value > 0);
        let (Some(items), Some(chars), Some(threads)) = (
            parse(&self.batch_items),
            parse(&self.batch_chars),
            parse(&self.batch_threads),
        ) else {
            self.warning =
                Some("Parti öğe, parti karakter ve thread pozitif tam sayı olmalı.".into());
            return false;
        };
        let mut updates = json!({
            "engine": engine,
            "batch": { "max_items": items, "max_chars": chars, "threads": threads },
        });
        updates[engine.as_str()] = Value::Object(section);
        match config::save(&updates, &self.config_path) {
            Ok(()) => {
                self.warning = None;
                true
            }
            Err(error) => {
                self.warning = Some(format!("Kaydedilemedi: {error:#}"));
                false
            }
        }
    }

    fn test_connection(&mut self) {
        if !self.save_engine() {
            return;
        }
        let cfg = match config::load(&self.config_path, &std::env::vars().collect()) {
            Ok(cfg) => cfg,
            Err(error) => {
                self.test_msg = format!("Ayarlar okunamadı: {error:#}");
                return;
            }
        };
        self.test_msg = "Bağlantı deneniyor…".into();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        thread::spawn(move || {
            let text = probe(cfg).unwrap_or_else(|error| format!("Bağlantı hatası: {error:#}"));
            let _ = tx.send(Msg::Notice(text));
            ctx.request_repaint();
        });
    }

    fn start_translate(&mut self, dry: bool) {
        if !self.save_engine() {
            return;
        }
        let text = self.limit.trim().to_owned();
        let Ok(limit) = text.parse::<usize>() else {
            self.warning = Some("Limit sayı olmalı.".into());
            return;
        };
        self.start_job(Job::Translate { dry, limit });
    }

    fn request_cancel(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        self.cancelling = true;
        self.log.push_str("Durdurma istendi.\n");
    }

    fn start_job(&mut self, job: Job) {
        if self.busy {
            self.warning = Some("Başka bir işlem sürüyor.".into());
            return;
        }
        if !self.save_game_exe() {
            return;
        }
        self.busy = true;
        self.cancelling = false;
        self.job_label = job.label();
        self.warning = None;
        self.progress = None;
        self.run_progress = None;
        self.run_started = Some(Instant::now());
        self.cancel.store(false, Ordering::SeqCst);
        self.log
            .push_str(&format!("\n--- {} başladı ---\n", job.label()));
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let logger: Logger = Arc::new(move |text: &str| {
            let _ = tx.send(Msg::Log(format!("{text}\n")));
            ctx.request_repaint();
        });
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let progress: ProgressFn = Arc::new(move |fraction: f32, text: &str| {
            let _ = tx.send(Msg::Progress {
                fraction,
                text: text.to_owned(),
            });
            ctx.request_repaint();
        });
        let reporter = Reporter {
            log: logger,
            progress,
            cancel: Arc::clone(&self.cancel),
        };
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let label = job.label();
        thread::spawn(move || {
            let result = run_job(job, &reporter).map_err(|error| format!("{error:#}"));
            let _ = tx.send(Msg::Done { label, result });
            ctx.request_repaint();
        });
    }

    fn reload_glossary(&mut self) {
        self.glossary = match Glossary::open(&state::glossary_path(self.lang)) {
            Ok(glossary) => {
                let mut rows: Vec<GlossRow> = glossary
                    .iter()
                    .map(|(term, entry)| GlossRow {
                        term: term.to_string(),
                        tr: entry.tr.clone(),
                        saved_tr: entry.tr.clone(),
                        locked: entry.locked,
                        auto: entry.auto,
                    })
                    .collect();
                rows.sort_by_key(|row| row.term.to_lowercase());
                rows
            }
            Err(error) => {
                self.g_msg = format!("Sözlük okunamadı: {error:#}");
                Vec::new()
            }
        };
    }

    /// Sözlük dosyasını açıp işlemi uygular, ardından listeyi yeniler.
    fn edit_glossary(&mut self, op: impl FnOnce(&mut Glossary) -> Result<String>) {
        if self.busy {
            self.g_msg = "Çeviri sürerken sözlük düzenlenemez.".into();
            return;
        }
        let result = Glossary::open(&state::glossary_path(self.lang)).and_then(|mut glossary| op(&mut glossary));
        match result {
            Ok(message) => {
                self.g_msg = message;
                self.reload_glossary();
                self.refresh_requested = true;
            }
            Err(error) => self.g_msg = format!("Hata: {error:#}"),
        }
    }

    fn apply_gloss_action(&mut self, index: usize, act: GlossAction) {
        let term = self.glossary[index].term.clone();
        let tr = self.glossary[index].tr.clone();
        let locked = self.glossary[index].locked;
        match act {
            GlossAction::Save => {
                self.edit_glossary(|glossary| {
                    glossary.add(&term, &tr, locked)?;
                    Ok(format!("Kaydedildi: {term}"))
                });
            }
            GlossAction::ToggleLock => {
                self.edit_glossary(|glossary| {
                    glossary.set_locked(&term, !locked)?;
                    Ok(if locked {
                        format!("Kilit açıldı: {term}")
                    } else {
                        format!("Kilitlendi: {term}")
                    })
                });
            }
            GlossAction::AskDelete => self.g_confirm_delete = Some(term),
            GlossAction::CancelDelete => self.g_confirm_delete = None,
            GlossAction::Delete => {
                self.g_confirm_delete = None;
                self.edit_glossary(|glossary| {
                    glossary.remove(&term)?;
                    Ok(format!("Silindi: {term}"))
                });
            }
        }
    }

    fn glossary_add(&mut self) {
        let (term, tr, locked) = (
            self.g_term.trim().to_owned(),
            self.g_tr.trim().to_owned(),
            self.g_lock_new,
        );
        let mut added = false;
        self.edit_glossary(|glossary| {
            glossary.add(&term, &tr, locked)?;
            added = true;
            Ok(format!("Eklendi: {term}"))
        });
        if added {
            self.g_term.clear();
            self.g_tr.clear();
        }
    }

    fn glossary_clean_preview(&mut self) {
        match Glossary::open(&state::glossary_path(self.lang)).and_then(|mut g| g.clean(true)) {
            Ok(report) if report.removed.is_empty() => {
                self.g_clean = None;
                self.g_msg = format!(
                    "Temizlenecek kayıt yok. {} geçersiz ama kilitli kayıt korunur.",
                    report.locked_kept
                );
            }
            Ok(report) => self.g_clean = Some(report.removed),
            Err(error) => self.g_msg = format!("Hata: {error:#}"),
        }
    }

    fn glossary_clean_confirm(&mut self) {
        self.g_clean = None;
        self.edit_glossary(|glossary| {
            let report = glossary.clean(false)?;
            Ok(format!(
                "{} kayıt silindi, {} kayıt kaldı, {} kilitli kayıt korundu.",
                report.removed.len(),
                report.kept,
                report.locked_kept
            ))
        });
    }

    fn reload_editor(&mut self) {
        let query = self.e_query.trim().to_owned();
        let filter = self.e_filter;
        let (limit, offset) = state::page_window(self.e_page, PAGE);
        let tm_filter = filter.to_tm(&self.engine);
        let result = TranslationMemory::open(&state::tm_path(self.lang))
            .and_then(|tm| tm.search(&query, &tm_filter, limit, offset));
        match result {
            Ok(mut rows) => {
                self.e_has_next = state::has_next_page(rows.len(), PAGE);
                rows.truncate(PAGE);
                self.editor = rows;
            }
            Err(error) => {
                self.editor.clear();
                self.e_has_next = false;
                self.e_msg = format!("Düzenleyici açılamadı: {error:#}");
            }
        }
        self.e_key = Some((query, filter, self.e_page));
    }

    fn save_manual(&mut self, source: String, text: String) {
        if self.busy {
            self.e_msg = "Çeviri sürerken sözlük düzenlenemez.".into();
            return;
        }
        if text.trim().is_empty() {
            self.e_msg = "Çeviri boş olamaz.".into();
            return;
        }
        if let Some(warning) = state::placeholder_warning(&source, &text) {
            if self.e_pending.as_deref() != Some(source.as_str()) {
                self.e_msg = format!("{warning} Yine de kaydetmek için düğmeye tekrar bas.");
                self.e_pending = Some(source);
                return;
            }
        }
        let result = TranslationMemory::open(&state::tm_path(self.lang))
            .and_then(|tm| tm.set_manual(&source, text.trim()));
        match result {
            Ok(()) => {
                self.e_pending = None;
                self.e_msg = "Kaydedildi. Oyunda görmek için 'Yeniden derle ve kur' çalıştır.".into();
                self.e_buffers.insert(source, text.trim().to_string());
                self.e_key = None;
            }
            Err(error) => self.e_msg = format!("Hata: {error:#}"),
        }
    }

    fn reset_entry(&mut self, source: String) {
        if self.busy {
            self.e_msg = "Çeviri sürerken sözlük düzenlenemez.".into();
            return;
        }
        let result = TranslationMemory::open(&state::tm_path(self.lang))
            .and_then(|tm| tm.reset(&source));
        match result {
            Ok(true) => {
                self.e_msg = "Sıfırlandı; metin bir sonraki çeviride yeniden çevrilecek.".into();
                self.e_buffers.remove(&source);
                self.e_key = None;
            }
            Ok(false) => self.e_msg = "Kayıt bulunamadı.".into(),
            Err(error) => self.e_msg = format!("Hata: {error:#}"),
        }
    }

    fn reload_backups(&mut self) {
        self.backups = backup::list(&config::project_dir(), self.lang.code).unwrap_or_default();
    }
}

#[derive(Clone, Copy)]
enum Style {
    Plain,
    Primary,
    Danger,
}

/// Eylem düğmesi: engelliyse nedeni ipucu olarak gösterilir. Tıklandıysa true döner.
fn action_button(
    ui: &mut egui::Ui,
    text: &str,
    action: Action,
    gate: Gate,
    style: Style,
) -> bool {
    let reason = state::block_reason(action, gate);
    let mut button = Button::new(text).min_size(egui::vec2(120.0, 36.0));
    match style {
        Style::Plain => {}
        Style::Primary => button = button.fill(ACCENT),
        Style::Danger => button = button.fill(DANGER),
    }
    let response = ui.add_enabled(reason.is_none(), button);
    let response = match reason {
        Some(text) => response.on_disabled_hover_text(text),
        None => response,
    };
    response.clicked()
}

const CARD_MIN_WIDTH: f32 = 150.0;
const CARD_MARGIN: f32 = 10.0;

fn card_grid(ui: &mut egui::Ui, cards: &[(&str, String, Option<Color32>)]) {
    let spacing = ui.spacing().item_spacing.x;
    let width = ui.available_width();
    let columns = ((width + spacing) / (CARD_MIN_WIDTH + spacing))
        .floor()
        .max(1.0) as usize;
    let card_width = (width - spacing * (columns as f32 - 1.0)) / columns as f32;
    for row in cards.chunks(columns) {
        ui.horizontal(|ui| {
            for (title, value, dot) in row {
                card(ui, card_width, title, value, *dot);
            }
        });
    }
}

fn card(ui: &mut egui::Ui, width: f32, title: &str, value: &str, dot: Option<Color32>) {
    egui::Frame::group(ui.style())
        .fill(PANEL)
        .rounding(8.0)
        .inner_margin(CARD_MARGIN)
        .show(ui, |ui| {
            ui.set_min_width(width - 2.0 * CARD_MARGIN - 2.0);
            ui.set_max_width(width - 2.0 * CARD_MARGIN - 2.0);
            ui.label(RichText::new(title).small().color(MUTED));
            ui.horizontal_wrapped(|ui| {
                if let Some(color) = dot {
                    ui.colored_label(color, "●");
                }
                ui.label(RichText::new(value).size(15.0).strong());
            });
        });
}

fn package_color(package: Package) -> Color32 {
    match package {
        Package::Missing => MUTED,
        Package::Inactive => AMBER,
        Package::Active => OK,
    }
}

fn initial_tab() -> Tab {
    match std::env::var("WUWALANG_TAB").as_deref() {
        Ok("Çeviri") => Tab::Translate,
        Ok("Sözlük") => Tab::Glossary,
        Ok("Düzenleyici") => Tab::Editor,
        Ok("Yedek") => Tab::Backup,
        Ok("Paketler") => Tab::Packs,
        _ => Tab::Status,
    }
}

fn probe(cfg: Config) -> Result<String> {
    let engine = make_engine(&cfg, Arc::new(UreqClient::new(Duration::from_secs(30))))?;
    let items = [Item {
        key: 1,
        id: "test".into(),
        text: "Hello".into(),
    }];
    let text = match engine.translate_batch(&items, &[], "") {
        Ok(result) => match result.translations.get(&1) {
            Some(translated) => format!("Bağlantı tamam: \"{translated}\""),
            None => "Bağlantı kuruldu ama yanıt boş.".to_string(),
        },
        Err(error) => format!("Bağlantı hatası: {error}"),
    };
    Ok(text)
}

fn resolve_root(game_exe: &str, game_path: &str) -> Result<PathBuf> {
    let exe = game_exe.trim();
    let root = if exe.is_empty() {
        PathBuf::from(game_path)
    } else {
        game::root_from_exe(Path::new(exe))?
    };
    ensure!(
        root.join(GAME_EXE).is_file(),
        "{GAME_EXE} bulunamadı: {}",
        root.display()
    );
    Ok(root)
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "Evet"
    } else {
        "Hayır"
    }
}

fn run_job(job: Job, rep: &Reporter) -> Result<Option<RunSummary>> {
    let cfg = config::load(&config::default_path(), &std::env::vars().collect())?;
    let log = &rep.log;
    match job {
        Job::Translate { dry, limit } => translate(cfg, dry, limit, rep),
        Job::Setup => prepare_tools(&config::tools_dir(), false, &|text: &str| log(text)).map(|()| None),
        Job::Build => {
            backup_before(&cfg, rep);
            build(cfg, rep).map(|()| None)
        }
        Job::Uninstall => {
            backup_before(&cfg, rep);
            make_project(&cfg, rep)?.uninstall()?;
            Ok(None)
        }
        Job::Rebuild => {
            backup_before(&cfg, rep);
            build(cfg.clone(), rep)?;
            make_project(&cfg, rep)?.install()?;
            Ok(None)
        }
        Job::Kur => install_all(cfg, rep).map(|()| None),
        Job::Toggle { enable } => {
            let project = make_project(&cfg, rep)?;
            if enable {
                project.enable()?;
            } else {
                project.disable()?;
            }
            Ok(None)
        }
        Job::ExportPack { dir } => export_pack(cfg, &dir, rep).map(|()| None),
        Job::ImportPack { dir } => import_pack(cfg, &dir, rep).map(|()| None),
        Job::PullPack { url } => pull_pack_job(cfg, &url, rep).map(|()| None),
        Job::Backup => manual_backup(&cfg, rep).map(|()| None),
        Job::Restore { file } => restore_backup(&cfg, &file, rep).map(|()| None),
    }
}

fn to_quads(records: &[Record]) -> Vec<(String, String, String, String)> {
    records
        .iter()
        .map(|r| {
            (
                r.file.clone(),
                r.table.clone(),
                r.id.clone(),
                r.source.clone(),
            )
        })
        .collect()
}

/// Değişiklik varsa çeviri belleğinin yedeğini alır; kaynak varsa topluluk paketini de yazar.
/// Yedek hatası işi durdurmaz.
fn backup_before(cfg: &Config, rep: &Reporter) {
    let log = &rep.log;
    let lang = state::active_lang(cfg);
    let rows = make_project(cfg, rep)
        .ok()
        .filter(|project| project.source_ready())
        .and_then(|project| project.rows().ok())
        .map(|records| to_quads(&records));
    let dir = state::pack_dir(lang);
    let pack = rows.as_deref().map(|rows| (dir.as_path(), rows));
    match backup::auto(&config::project_dir(), lang.code, &state::tm_path(lang), pack) {
        Ok(Some(info)) => log(&format!("Yedek alındı: {}", info.path.display())),
        Ok(None) => {}
        Err(error) => log(&format!("Uyarı: yedek alınamadı: {error:#}")),
    }
}

fn manual_backup(cfg: &Config, rep: &Reporter) -> Result<()> {
    let lang = state::active_lang(cfg);
    match backup::create(&config::project_dir(), lang.code, &state::tm_path(lang), true)? {
        Some(info) => (rep.log)(&format!("Yedek alındı: {}", info.path.display())),
        None => (rep.log)("Yedeklenecek çeviri belleği yok."),
    }
    Ok(())
}

fn restore_backup(cfg: &Config, file: &Path, rep: &Reporter) -> Result<()> {
    let lang = state::active_lang(cfg);
    let pre = backup::restore(&config::project_dir(), lang.code, file, &state::tm_path(lang))?;
    match pre {
        Some(info) => (rep.log)(&format!(
            "Geri yüklendi. Önceki çeviri belleği yedeklendi: {}",
            info.path.display()
        )),
        None => (rep.log)("Geri yüklendi."),
    }
    Ok(())
}

fn export_pack(cfg: Config, dir: &Path, rep: &Reporter) -> Result<()> {
    let log = &rep.log;
    let project = make_project(&cfg, rep)?;
    ensure_source(&project, log)?;
    let quads = to_quads(&project.rows()?);
    let tm = TranslationMemory::open(&state::tm_path(state::active_lang(&cfg)))?;
    let count = tm.export_pack(dir, &quads)?;
    log(&format!("{count} satır dışa aktarıldı: {}", dir.display()));
    Ok(())
}

fn import_pack(cfg: Config, dir: &Path, rep: &Reporter) -> Result<()> {
    let log = &rep.log;
    let project = make_project(&cfg, rep)?;
    ensure_source(&project, log)?;
    let quads = to_quads(&project.rows()?);
    let tm = TranslationMemory::open(&state::tm_path(state::active_lang(&cfg)))?;
    let stats = tm.import_pack(dir, Some(quads.as_slice()))?;
    log(&format!(
        "Paket içe aktarıldı: {} satır alındı, {} atlandı.",
        stats.imported, stats.skipped
    ));
    Ok(())
}

fn pull_pack_job(cfg: Config, url: &str, rep: &Reporter) -> Result<()> {
    let log = &rep.log;
    let project = make_project(&cfg, rep)?;
    let quads = if project.source_ready() {
        Some(to_quads(&project.rows()?))
    } else {
        None
    };
    let tm = TranslationMemory::open(&state::tm_path(state::active_lang(&cfg)))?;
    let url = lang::pack_url_for(url, state::active_lang(&cfg));
    let stats = pull_pack(&url, &tm, quads.as_deref(), &|text: &str| log(text))?;
    log(&format!(
        "Repodan çekildi: {} dosya tarandı, {} indirildi, {} satır alındı, {} atlandı.",
        stats.files, stats.downloaded, stats.imported, stats.skipped
    ));
    Ok(())
}

fn make_project(cfg: &Config, rep: &Reporter) -> Result<Project> {
    let forward = Arc::clone(&rep.log);
    let project = Project::new(
        game::game_dir(cfg)?,
        config::project_dir(),
        config::tools_dir(),
    )?;
    Ok(project
        .with_log(move |text| forward(text))
        .with_cancel(Arc::clone(&rep.cancel)))
}

fn install_all(cfg: Config, rep: &Reporter) -> Result<()> {
    const STEPS: usize = 5;
    let log = &rep.log;

    rep.step(1, STEPS, "Araçlar hazırlanıyor");
    prepare_tools(&config::tools_dir(), false, &|text: &str| log(text))?;

    backup_before(&cfg, rep);

    rep.step(2, STEPS, "Oyun verisi çıkarılıyor");
    let project = make_project(&cfg, rep)?;
    ensure_source(&project, log)?;
    let records = project.rows()?;

    rep.step(3, STEPS, "Topluluk paketi uygulanıyor");
    let quads = to_quads(&records);
    let tm = TranslationMemory::open(&state::tm_path(state::active_lang(&cfg)))?;
    let imported = tm.import_pack(&state::pack_dir(state::active_lang(&cfg)), Some(quads.as_slice()))?;
    log(&format!(
        "Topluluk paketi: {} satır alındı, {} atlandı.",
        imported.imported, imported.skipped
    ));
    let url = lang::pack_url_for(&cfg.pack_url, state::active_lang(&cfg));
    if !url.is_empty() {
        match pull_pack(&url, &tm, Some(quads.as_slice()), &|text: &str| log(text)) {
            Ok(stats) => log(&format!(
                "Repodan paket: {} dosya indirildi, {} satır alındı.",
                stats.downloaded, stats.imported
            )),
            Err(error) => log(&format!(
                "Uyarı: repodan paket çekilemedi ({error:#}). Yerel paket kullanılmaya devam ediyor."
            )),
        }
    }

    rep.step(4, STEPS, "Paket derleniyor");
    let sources: Vec<&str> = records.iter().map(|r| r.source.as_str()).collect();
    let hits = tm.get_many(&sources)?;
    ensure!(
        !hits.is_empty(),
        "Önce çeviri gerekli: Çeviri sekmesinden API anahtarı girip çevir. Boş paket kurulmadı."
    );
    let path = project.build(|_, _, _, source| hits.get(source).cloned())?;
    log(&format!("Paket hazır: {}", path.display()));

    rep.step(5, STEPS, "Oyuna kuruluyor");
    project.install()?;
    match state::coverage(&records, Some(&tm)) {
        Ok(coverage) => log(&format!(
            "Kuruldu. Oyunun çevrili kısmı Türkçe, geri kalanı İngilizce görünür (%{} çevrili).",
            format!("{:.1}", coverage.percent).replace('.', ",")
        )),
        Err(_) => log("Kuruldu. Oyunun çevrili kısmı Türkçe, geri kalanı İngilizce görünür."),
    }
    Ok(())
}

fn ensure_source(project: &Project, log: &Logger) -> Result<()> {
    match project.extract(false) {
        Ok(_) => Ok(()),
        Err(error) if project.source_ready() => {
            log(&format!(
                "Uyarı: {error:#} Önbellekteki kaynak veritabanları kullanılıyor."
            ));
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn probe_config(mut cfg: Config) -> Config {
    fn fill(value: &mut String) {
        if value.is_empty() {
            *value = "dry-run".into();
        }
    }
    fill(&mut cfg.yandex.api_key);
    fill(&mut cfg.yandex.folder_id);
    fill(&mut cfg.google.api_key);
    fill(&mut cfg.openai.api_key);
    fill(&mut cfg.openai.model);
    fill(&mut cfg.anthropic.api_key);
    fill(&mut cfg.anthropic.model);
    cfg
}

fn trim(mut plan: Plan, limit: usize) -> Plan {
    let batches = std::mem::take(&mut plan.batches);
    let mut kept = Vec::new();
    let mut count = 0;
    for mut batch in batches {
        if count >= limit {
            break;
        }
        if count + batch.items.len() > limit {
            let keep = limit - count;
            batch.items.truncate(keep);
            batch.sources.truncate(keep);
            batch.rids.truncate(keep);
            batch.maps.truncate(keep);
            batch.chars = batch.items.iter().map(|item| item.text.len()).sum();
        }
        count += batch.items.len();
        kept.push(batch);
    }
    plan.batches = kept;
    plan
}

fn translate(cfg: Config, dry: bool, limit: usize, rep: &Reporter) -> Result<Option<RunSummary>> {
    let log = &rep.log;
    let project = make_project(&cfg, rep)?;
    ensure_source(&project, log)?;
    let records = project.rows()?;
    let engine_cfg = if dry {
        probe_config(cfg.clone())
    } else {
        cfg.clone()
    };
    let engine = make_engine(
        &engine_cfg,
        Arc::new(UreqClient::new(Duration::from_secs(120))),
    )?;
    let tm = TranslationMemory::open(&state::tm_path(state::active_lang(&cfg)))?;
    let mut glossary = Glossary::open(&state::glossary_path(state::active_lang(&cfg)))?;
    let mut plan = batching::plan(
        &records,
        &tm,
        &glossary,
        engine.as_ref(),
        cfg.batch.max_items,
        cfg.batch.max_chars,
    )?;
    if limit > 0 {
        plan = trim(plan, limit);
    }
    if dry {
        log(&format!("{:#?}", batching::dry_run(&plan, engine.as_ref())));
        return Ok(None);
    }
    let sleep = |delay: Duration| thread::sleep(delay);
    let emit = |text: &str| log(text);
    let options = RunOptions {
        threads: cfg.batch.threads,
        cancelled: Some(rep.cancel.as_ref()),
        log: &emit,
        sleep: &sleep,
        attempts: 5,
    };
    let outcome = batching::run(plan, engine.as_ref(), &tm, &mut glossary, &options);
    backup_before(&cfg, rep);
    outcome.map(Some)
}

fn build(cfg: Config, rep: &Reporter) -> Result<()> {
    let log = &rep.log;
    let project = make_project(&cfg, rep)?;
    ensure_source(&project, log)?;
    let records = project.rows()?;
    let tm = TranslationMemory::open(&state::tm_path(state::active_lang(&cfg)))?;
    let sources: Vec<&str> = records
        .iter()
        .map(|record| record.source.as_str())
        .collect();
    let hits = tm.get_many(&sources)?;
    let path = project.build(|_file, _table, _id, source| hits.get(source).cloned())?;
    log(&format!("Paket hazır: {}", path.display()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn resolve_root_accepts_shipping_exe_and_plain_game_dir() {
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path().join("Wuthering Waves Game");
        fs::create_dir_all(game.join("Client/Binaries/Win64")).unwrap();
        fs::create_dir_all(game.join("Client/Saved/Resources")).unwrap();
        fs::create_dir_all(game.join("Engine")).unwrap();
        fs::write(game.join(GAME_EXE), b"").unwrap();
        let shipping = game.join("Client/Binaries/Win64").join(game::SHIPPING_EXE);
        fs::write(&shipping, b"").unwrap();

        assert_eq!(resolve_root(&shipping.to_string_lossy(), "").unwrap(), game);
        assert_eq!(resolve_root("", &game.to_string_lossy()).unwrap(), game);
        assert!(resolve_root("", &dir.path().join("yok").to_string_lossy()).is_err());
    }

    #[test]
    fn package_colors_are_distinct() {
        assert_ne!(package_color(Package::Missing), package_color(Package::Active));
        assert_ne!(package_color(Package::Inactive), package_color(Package::Active));
    }
}
