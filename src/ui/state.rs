use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{ensure, Result};

use crate::batching::{is_translatable, Record};
use crate::config::{self, Config};
use crate::game::{self, Project};
use crate::glossary::Glossary;
use crate::lang::{self, Lang};
use crate::placeholders::{placeholders_of, validate_placeholders};
use crate::tm::{Filter as TmFilter, TranslationMemory};

#[cfg(windows)]
const CLIENT_EXE: &str = "Client-Win64-Shipping.exe";

pub type Rect = (i32, i32, i32, i32);

/// Paketin durumu: kurulu değil, kurulu ama pasif, kurulu ve aktif.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Package {
    Missing,
    Inactive,
    Active,
}

impl Package {
    pub fn of(installed: bool, enabled: bool) -> Self {
        match (installed, enabled) {
            (false, _) => Package::Missing,
            (true, false) => Package::Inactive,
            (true, true) => Package::Active,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Package::Missing => "Kurulu değil",
            Package::Inactive => "Kurulu, pasif",
            Package::Active => "Kurulu, aktif",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub game_running: bool,
    pub installed: bool,
    pub enabled: bool,
    pub tm_count: usize,
    pub glossary_count: usize,
    pub engine: Option<String>,
    pub lang: String,
}

impl Status {
    pub fn package(&self) -> Package {
        Package::of(self.installed, self.enabled)
    }
}

/// Yapılandırmadaki hedef dil. Geçersizse varsayılan dil.
pub fn active_lang(cfg: &Config) -> &'static Lang {
    lang::validate(&cfg.target).unwrap_or(lang::default_lang())
}

pub fn tm_path(lang: &Lang) -> PathBuf {
    lang::tm_path(&config::project_dir(), lang)
}

pub fn glossary_path(lang: &Lang) -> PathBuf {
    lang::glossary_path(&config::project_dir(), lang)
}

pub fn pack_dir(lang: &Lang) -> PathBuf {
    lang::pack_dir(&config::project_dir(), lang)
}

pub fn cache_dir(lang: &Lang) -> PathBuf {
    lang::cache_dir(&config::project_dir(), lang)
}

pub fn load_config(path: &Path) -> Option<Config> {
    let env: HashMap<String, String> = std::env::vars().collect();
    config::load(path, &env).ok()
}

pub fn get_status(config_path: &Path) -> Status {
    let cfg = load_config(config_path);
    let defaults = Config::default();
    let current = cfg.as_ref().unwrap_or(&defaults);
    let lang = active_lang(current);
    let game_dir = game::game_dir(current).unwrap_or_else(|_| PathBuf::from(&current.game_path));
    Status {
        game_running: game::game_running(),
        installed: game::is_installed(&game_dir),
        enabled: game::is_enabled(&game_dir),
        tm_count: tm_count(&tm_path(lang)).unwrap_or(0),
        glossary_count: Glossary::open(&glossary_path(lang))
            .map(|g| g.len())
            .unwrap_or(0),
        engine: cfg.as_ref().map(|c| c.engine.clone()),
        lang: lang.code.to_string(),
    }
}

fn tm_count(path: &Path) -> anyhow::Result<usize> {
    if !path.is_file() {
        return Ok(0);
    }
    TranslationMemory::open(path)?.count()
}

pub fn overlay_label(package: Package, lang_code: &str) -> String {
    match package {
        Package::Active => format!("WuwaLang aktif ({lang_code}) · F7"),
        Package::Inactive => "WuwaLang pasif · F7".to_string(),
        Package::Missing => "WuwaLang kurulu değil · F7".to_string(),
    }
}

pub fn overlay_text(status: &Status) -> Option<String> {
    if !status.game_running {
        return None;
    }
    Some(overlay_label(status.package(), &status.lang))
}

/// Menüdeki eylemler ve engelleyen koşullar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Install,
    Remove,
    Toggle,
    Rebuild,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gate {
    pub busy: bool,
    pub game_running: bool,
    pub root_ok: bool,
    pub installed: bool,
    pub accepted_risk: bool,
}

/// Eylem engelliyse nedenini döndürür (düğme ipucu olarak gösterilir).
pub fn block_reason(action: Action, gate: Gate) -> Option<&'static str> {
    if gate.busy {
        return Some("Başka bir işlem sürüyor.");
    }
    if gate.game_running {
        return Some("Oyun açık. Önce oyunu kapat.");
    }
    if !gate.root_ok {
        return Some("Oyun yolu geçersiz. Wuthering Waves exe dosyasını seç.");
    }
    match action {
        Action::Install | Action::Rebuild if !gate.accepted_risk => {
            Some("Kur için Fair Play onayını işaretle.")
        }
        Action::Toggle if !gate.installed => Some("Paket kurulu değil. Önce Kur."),
        _ => None,
    }
}

/// Tamamlanan oran, 0 ile 1 arasında.
pub fn fraction(done: usize, total: usize) -> f32 {
    if total == 0 {
        return 0.0;
    }
    (done as f32 / total as f32).clamp(0.0, 1.0)
}

/// Tahmini kalan süre. Henüz ilerleme yoksa None.
pub fn eta(elapsed: Duration, done: usize, total: usize) -> Option<Duration> {
    if done == 0 || total == 0 {
        return None;
    }
    if done >= total {
        return Some(Duration::ZERO);
    }
    let per_unit = elapsed.as_secs_f64() / done as f64;
    Some(Duration::from_secs_f64(per_unit * (total - done) as f64))
}

pub fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    let (hours, minutes, seconds) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if hours > 0 {
        format!("{hours} sa {minutes:02} dk")
    } else if minutes > 0 {
        format!("{minutes} dk {seconds:02} sn")
    } else {
        format!("{seconds} sn")
    }
}

/// Çeviri günlüğündeki "Paket d/t tamamlandı." satırını (tamamlanan, toplam) olarak okur.
pub fn parse_batch_progress(line: &str) -> Option<(usize, usize)> {
    let rest = line
        .trim()
        .strip_prefix("Paket ")?
        .strip_suffix(" tamamlandı.")?;
    let (done, total) = rest.split_once('/')?;
    Some((done.parse().ok()?, total.parse().ok()?))
}

/// Oyunun çevrilme durumu: çevrilebilir benzersiz metinler ve TM'de karşılığı olanlar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coverage {
    pub translated: usize,
    pub total: usize,
    pub percent: f64,
}

impl Coverage {
    pub fn new(translated: usize, total: usize) -> Self {
        Self {
            translated,
            total,
            percent: percent(translated, total),
        }
    }
}

/// Yüzde, bir ondalık basamağa yuvarlanır. Payda 0 ise 0.
pub fn percent(translated: usize, total: usize) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let tenths = (translated.min(total) as f64 * 1000.0 / total as f64).round();
    tenths / 10.0
}

/// Üst çubuktaki metin. Ölçüm yoksa hata metni ya da "hesaplanıyor" yazılır.
pub fn coverage_text(coverage: Option<&Coverage>, error: Option<&str>) -> String {
    match (coverage, error) {
        (Some(c), _) => format!(
            "Oyunun %{}'si çevrildi ({} / {})",
            format!("{:.1}", c.percent).replace('.', ","),
            c.translated,
            c.total
        ),
        (None, Some(message)) => message.to_string(),
        (None, None) => "Yüzde hesaplanıyor…".to_string(),
    }
}

/// Oyun veritabanlarındaki benzersiz çevrilebilir metinleri ve TM'deki karşılıklarını sayar.
/// Boş, sayı ya da etiket satırları (batching'in atladığı) sayılmaz.
pub fn coverage(records: &[Record], tm: Option<&TranslationMemory>) -> Result<Coverage> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut sources: Vec<String> = Vec::new();
    for record in records {
        if is_translatable(&record.source) && seen.insert(record.source.as_str()) {
            sources.push(record.source.clone());
        }
    }
    let translated = match tm {
        Some(tm) if !sources.is_empty() => tm.get_many(&sources)?.len(),
        _ => 0,
    };
    Ok(Coverage::new(translated, sources.len()))
}

/// Hedef dilin çıkarılmış oyun verisinden ve TM'den yüzdeyi ölçer (ağır iş; arka planda çağrılır).
pub fn measure(lang: &Lang) -> Result<Coverage> {
    let project = Project::new(
        PathBuf::new(),
        config::project_dir(),
        config::tools_dir(),
    )?;
    ensure!(project.source_ready(), "Önce çıkarma gerekli");
    let records = project.rows()?;
    let path = tm_path(lang);
    let tm = if path.is_file() {
        Some(TranslationMemory::open(&path)?)
    } else {
        None
    };
    coverage(&records, tm.as_ref())
}

/// Düzenleyici filtresi.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorFilter {
    All,
    Manual,
    Engine,
}

impl EditorFilter {
    pub const ALL: [EditorFilter; 3] = [EditorFilter::All, EditorFilter::Manual, EditorFilter::Engine];

    pub fn label(self) -> &'static str {
        match self {
            EditorFilter::All => "Hepsi",
            EditorFilter::Manual => "Elle düzenlenenler",
            EditorFilter::Engine => "Motor",
        }
    }

    pub fn to_tm(self, engine: &str) -> TmFilter {
        match self {
            EditorFilter::All => TmFilter::All,
            EditorFilter::Manual => TmFilter::Manual,
            EditorFilter::Engine => TmFilter::Engine(engine.to_string()),
        }
    }
}

/// Sayfa penceresi: (çekilecek kayıt sayısı, atlanacak kayıt sayısı).
/// Fazladan bir kayıt, sonraki sayfanın var olup olmadığını gösterir.
pub fn page_window(page: usize, per_page: usize) -> (usize, usize) {
    (per_page + 1, page * per_page)
}

pub fn has_next_page(fetched: usize, per_page: usize) -> bool {
    fetched > per_page
}

/// Çeviri kullanım özetini (ör. token sayıları) tek satır yapar.
pub fn usage_text(usage: &BTreeMap<String, u64>) -> String {
    if usage.is_empty() {
        return "Kullanım bilgisi yok".to_string();
    }
    usage
        .iter()
        .map(|(key, value)| format!("{key}: {value}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Etkinleştir/devre dışı bırak düğmesinin etiketi.
pub fn toggle_label(enabled: bool) -> &'static str {
    if enabled {
        "Devre dışı bırak"
    } else {
        "Aktif et"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlossRow {
    pub term: String,
    pub tr: String,
    pub saved_tr: String,
    pub locked: bool,
    pub auto: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlossFilter {
    All,
    Locked,
    Unlocked,
    FromModel,
}

impl GlossFilter {
    pub const ALL: [GlossFilter; 4] = [
        GlossFilter::All,
        GlossFilter::Locked,
        GlossFilter::Unlocked,
        GlossFilter::FromModel,
    ];

    pub fn label(self) -> &'static str {
        match self {
            GlossFilter::All => "Hepsi",
            GlossFilter::Locked => "Kilitli",
            GlossFilter::Unlocked => "Kilitsiz",
            GlossFilter::FromModel => "Modelin eklediği",
        }
    }
}

/// Sözlük satırlarından filtre ve aramaya uyanların indeksleri (sıra korunur).
/// Arama terimde ve karşılıkta yapılır, büyük/küçük harf duyarsızdır.
pub fn filter_glossary(rows: &[GlossRow], query: &str, filter: GlossFilter) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    rows.iter()
        .enumerate()
        .filter(|(_, row)| {
            let kind_ok = match filter {
                GlossFilter::All => true,
                GlossFilter::Locked => row.locked,
                GlossFilter::Unlocked => !row.locked,
                GlossFilter::FromModel => row.auto,
            };
            kind_ok
                && (needle.is_empty()
                    || row.term.to_lowercase().contains(&needle)
                    || row.tr.to_lowercase().contains(&needle))
        })
        .map(|(index, _)| index)
        .collect()
}

/// Kaynak ve çevirideki yer tutucu kümeleri eşleşmiyorsa uyarı metni.
pub fn placeholder_warning(source: &str, translation: &str) -> Option<String> {
    if validate_placeholders(source, translation) {
        return None;
    }
    Some(format!(
        "Yer tutucular eşleşmiyor. Kaynakta: {} · çeviride: {}.",
        describe_placeholders(source),
        describe_placeholders(translation)
    ))
}

fn describe_placeholders(text: &str) -> String {
    let found = placeholders_of(text);
    if found.is_empty() {
        return "yok".to_string();
    }
    found
        .iter()
        .map(|(tag, count)| format!("{tag} x{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(windows)]
fn client_pids() -> Vec<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let mut pids = Vec::new();
    // SAFETY: the snapshot handle is checked and closed; the entry is zeroed and its size set before use.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return pids;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snapshot, &mut entry) != 0;
        while more {
            let end = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..end]).eq_ignore_ascii_case(CLIENT_EXE) {
                pids.push(entry.th32ProcessID);
            }
            more = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    pids
}

#[cfg(windows)]
struct WindowSearch {
    pids: Vec<u32>,
    best: Option<(i64, Rect)>,
}

#[cfg(windows)]
unsafe extern "system" fn visit_window(
    hwnd: windows_sys::Win32::Foundation::HWND,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::BOOL {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindow, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible, GW_OWNER,
    };

    // SAFETY: lparam is the &mut WindowSearch passed to EnumWindows, alive for the whole call.
    let search = &mut *(lparam as *mut WindowSearch);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if search.pids.contains(&pid)
        && IsWindowVisible(hwnd) != 0
        && IsIconic(hwnd) == 0
        && GetWindow(hwnd, GW_OWNER).is_null()
    {
        let mut rect: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut rect) != 0 {
            let width = i64::from(rect.right - rect.left);
            let height = i64::from(rect.bottom - rect.top);
            let area = width * height;
            if width > 0 && height > 0 && search.best.is_none_or(|(best, _)| area > best) {
                search.best = Some((area, (rect.left, rect.top, rect.right, rect.bottom)));
            }
        }
    }
    1
}

#[cfg(windows)]
pub fn find_game_rect() -> Option<Rect> {
    use windows_sys::Win32::Foundation::LPARAM;
    use windows_sys::Win32::UI::WindowsAndMessaging::EnumWindows;

    let pids = client_pids();
    if pids.is_empty() {
        return None;
    }
    let mut search = WindowSearch { pids, best: None };
    // SAFETY: visit_window only touches the search state through the pointer valid for this call.
    unsafe {
        EnumWindows(
            Some(visit_window),
            &mut search as *mut WindowSearch as LPARAM,
        );
    }
    search.best.map(|(_, rect)| rect)
}

#[cfg(not(windows))]
pub fn find_game_rect() -> Option<Rect> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(running: bool, installed: bool, enabled: bool) -> Status {
        Status {
            game_running: running,
            installed,
            enabled,
            tm_count: 0,
            glossary_count: 0,
            engine: None,
            lang: "tr".into(),
        }
    }

    #[test]
    fn overlay_text_follows_package_state() {
        assert_eq!(
            overlay_text(&status(true, true, true)).as_deref(),
            Some("WuwaLang aktif (tr) · F7")
        );
        assert_eq!(
            overlay_text(&status(true, true, false)).as_deref(),
            Some("WuwaLang pasif · F7")
        );
        assert_eq!(
            overlay_text(&status(true, false, true)).as_deref(),
            Some("WuwaLang kurulu değil · F7")
        );
        assert_eq!(overlay_text(&status(false, true, true)), None);
    }

    #[test]
    fn package_state_and_labels() {
        assert_eq!(Package::of(false, true), Package::Missing);
        assert_eq!(Package::of(true, false), Package::Inactive);
        assert_eq!(Package::of(true, true), Package::Active);
        assert_eq!(status(false, true, true).package().label(), "Kurulu, aktif");
        assert_eq!(overlay_label(Package::Active, "de"), "WuwaLang aktif (de) · F7");
    }

    #[test]
    fn gate_explains_first_blocker() {
        let open = Gate {
            busy: false,
            game_running: false,
            root_ok: true,
            installed: true,
            accepted_risk: true,
        };
        assert_eq!(block_reason(Action::Install, open), None);
        assert_eq!(block_reason(Action::Toggle, open), None);
        let busy = Gate { busy: true, ..open };
        assert_eq!(block_reason(Action::Remove, busy), Some("Başka bir işlem sürüyor."));
        let running = Gate {
            game_running: true,
            ..open
        };
        assert!(block_reason(Action::Toggle, running).unwrap().contains("oyunu kapat"));
        let no_risk = Gate {
            accepted_risk: false,
            ..open
        };
        assert!(block_reason(Action::Install, no_risk).is_some());
        assert_eq!(block_reason(Action::Remove, no_risk), None);
        let missing = Gate {
            installed: false,
            ..open
        };
        assert!(block_reason(Action::Toggle, missing).unwrap().contains("Önce Kur"));
    }

    #[test]
    fn eta_and_percent_are_sane() {
        assert_eq!(fraction(0, 0), 0.0);
        assert_eq!(fraction(3, 4), 0.75);
        assert_eq!(fraction(9, 4), 1.0);
        assert_eq!(eta(Duration::from_secs(10), 0, 5), None);
        assert_eq!(eta(Duration::from_secs(10), 5, 5), Some(Duration::ZERO));
        assert_eq!(
            eta(Duration::from_secs(10), 2, 6),
            Some(Duration::from_secs(20))
        );
        assert_eq!(format_duration(Duration::from_secs(42)), "42 sn");
        assert_eq!(format_duration(Duration::from_secs(125)), "2 dk 05 sn");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1 sa 02 dk");
    }

    #[test]
    fn percent_rounds_to_one_decimal_and_survives_zero() {
        assert_eq!(percent(0, 0), 0.0);
        assert_eq!(percent(1, 3), 33.3);
        assert_eq!(percent(2, 3), 66.7);
        assert_eq!(percent(3, 3), 100.0);
        assert_eq!(percent(0, 5), 0.0);
        assert_eq!(coverage_text(None, Some("Önce çıkarma gerekli")), "Önce çıkarma gerekli");
        assert_eq!(coverage_text(None, None), "Yüzde hesaplanıyor…");
        assert_eq!(
            coverage_text(Some(&Coverage::new(1, 3)), None),
            "Oyunun %33,3'si çevrildi (1 / 3)"
        );
    }

    #[test]
    fn coverage_counts_unique_translatable_sources_only() {
        let dir = tempfile::tempdir().unwrap();
        let tm = TranslationMemory::open(&dir.path().join("tm.db")).unwrap();
        tm.put_many(&[("Rover", "Gezgin", "yandex")]).unwrap();
        let record = |source: &str| Record {
            file: "f".into(),
            table: "t".into(),
            id: source.into(),
            source: source.into(),
        };
        let records = vec![
            record("Rover"),
            record("Rover"),
            record("Hit {0}"),
            record("3"),
            record("   "),
        ];
        let with_tm = coverage(&records, Some(&tm)).unwrap();
        assert_eq!(with_tm, Coverage::new(1, 2));
        assert_eq!(with_tm.percent, 50.0);
        assert_eq!(coverage(&records, None).unwrap(), Coverage::new(0, 2));
        assert_eq!(coverage(&[], None).unwrap().percent, 0.0);
    }

    #[test]
    fn editor_filters_map_to_tm_filters_and_pages_fetch_one_extra() {
        assert_eq!(EditorFilter::All.to_tm("yandex"), TmFilter::All);
        assert_eq!(EditorFilter::Manual.to_tm("yandex"), TmFilter::Manual);
        assert_eq!(
            EditorFilter::Engine.to_tm("google"),
            TmFilter::Engine("google".into())
        );
        assert_eq!(EditorFilter::Manual.label(), "Elle düzenlenenler");
        assert_eq!(page_window(0, 50), (51, 0));
        assert_eq!(page_window(2, 50), (51, 100));
        assert!(has_next_page(51, 50));
        assert!(!has_next_page(50, 50));
    }

    #[test]
    fn usage_and_toggle_labels() {
        assert_eq!(usage_text(&BTreeMap::new()), "Kullanım bilgisi yok");
        let usage = BTreeMap::from([("input".to_string(), 10), ("output".to_string(), 4)]);
        assert_eq!(usage_text(&usage), "input: 10 · output: 4");
        assert_eq!(toggle_label(true), "Devre dışı bırak");
        assert_eq!(toggle_label(false), "Aktif et");
    }

    #[test]
    fn batch_progress_line_is_parsed() {
        assert_eq!(parse_batch_progress("Paket 3/10 tamamlandı.\n"), Some((3, 10)));
        assert_eq!(parse_batch_progress("Paket başarısız: x"), None);
        assert_eq!(parse_batch_progress("Paket a/b tamamlandı."), None);
    }

    fn row(term: &str, tr: &str, locked: bool, auto: bool) -> GlossRow {
        GlossRow {
            term: term.into(),
            tr: tr.into(),
            saved_tr: tr.into(),
            locked,
            auto,
        }
    }

    #[test]
    fn glossary_filter_and_search() {
        let rows = vec![
            row("Rover", "Gezgin", true, false),
            row("Jiyan", "Jiyan", false, true),
            row("Lupa", "Büyüteç", false, false),
        ];
        assert_eq!(filter_glossary(&rows, "", GlossFilter::All), vec![0, 1, 2]);
        assert_eq!(filter_glossary(&rows, "", GlossFilter::Locked), vec![0]);
        assert_eq!(filter_glossary(&rows, "", GlossFilter::Unlocked), vec![1, 2]);
        assert_eq!(filter_glossary(&rows, "", GlossFilter::FromModel), vec![1]);
        assert_eq!(filter_glossary(&rows, "GEZ", GlossFilter::All), vec![0]);
        assert_eq!(filter_glossary(&rows, "ROVER", GlossFilter::Unlocked), Vec::<usize>::new());
        assert_eq!(filter_glossary(&rows, "büyüteç", GlossFilter::All), vec![2]);
    }

    #[test]
    fn placeholder_warning_compares_sets() {
        assert_eq!(placeholder_warning("Hit {0}", "Vuruş {0}"), None);
        assert_eq!(placeholder_warning("Hit {0}", "Vuruş"), Some(
            "Yer tutucular eşleşmiyor. Kaynakta: {0} x1 · çeviride: yok.".to_string()
        ));
        assert!(placeholder_warning("Hit {0} {1}", "Vuruş {0} {0}").is_some());
    }

    #[test]
    fn get_status_runs_without_windows() {
        let dir = tempfile::tempdir().unwrap();
        let result = get_status(&dir.path().join("missing.json"));
        assert!(!result.game_running);
        assert!(!result.installed);
        assert_eq!(result.lang, "tr");
    }

    #[test]
    #[cfg(not(windows))]
    fn find_game_rect_is_none_off_windows() {
        assert_eq!(find_game_rect(), None);
    }
}
