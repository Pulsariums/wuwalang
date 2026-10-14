use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use clap::builder::PossibleValuesParser;
use clap::{Parser, Subcommand};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags};

use wuwalang::backup;
use wuwalang::batching::{self, Batch, Plan, Record, RunOptions};
use wuwalang::config::{self, project_dir, Config, ENGINES};
use wuwalang::engines::{make_engine, Engine, UreqClient};
use wuwalang::game::{game_dir, prepare_tools, Project};
use wuwalang::glossary::{Entry, Glossary};
use wuwalang::lang::{self, Lang};
use wuwalang::tm::{pull_pack_in, TranslationMemory};

const ENGINE_TIMEOUT: Duration = Duration::from_secs(180);
const RUN_ATTEMPTS: u32 = 5;

/// (dosya, tablo, kimlik, kaynak metin)
type Row = (String, String, String, String);

#[derive(Parser)]
#[command(name = "wuwalang", about = "WuwaLang çeviri aracı")]
struct Cli {
    /// config.json yolu (varsayılan: proje kökü)
    #[arg(long)]
    config: Option<PathBuf>,

    /// Hedef dil kodu (ör. tr, de, pt-BR); config.json target alanını geçersiz kılar
    #[arg(long, global = true)]
    lang: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Araçları hazırla: FModelCLI ve kaynak anahtarları
    Setup {
        /// Anahtarları oyun güncellemesinden sonra yeniden indir
        #[arg(long)]
        refresh_keys: bool,
    },
    /// Oyundan İngilizce veritabanlarını çıkar
    Extract {
        /// Sürüm değişmese de yeniden çıkar
        #[arg(long)]
        force: bool,
    },
    /// Eksik metinleri çevir
    Translate {
        #[arg(long, value_parser = PossibleValuesParser::new(ENGINES))]
        engine: Option<String>,
        /// En fazla N benzersiz metin
        #[arg(long, default_value_t = 0)]
        limit: usize,
        /// API çağırmadan tahmin yazdır
        #[arg(long)]
        dry_run: bool,
    },
    /// Çeviri belleğinden Türkçe paketi derle
    Build,
    /// Paketi oyuna kur
    Install,
    /// Paketi oyundan kaldır
    Uninstall,
    /// Kurulu Türkçe paketi devre dışı bırak (paket dosyası kalır)
    Disable,
    /// Devre dışı bırakılmış Türkçe paketi yeniden etkinleştir
    Enable,
    /// Desteklenen hedef dilleri listele (* etkin dil)
    Langs,
    /// Topluluk paketini çeviri belleğine al
    ImportPack {
        /// Paket klasörü (varsayılan: packs/<dil>)
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Çeviri belleğini topluluk paketi olarak dışa aktar
    ExportPack {
        /// Çıktı klasörü (varsayılan: packs/<dil>)
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Topluluk paketini repodan çek ve çeviri belleğine al
    PullPack {
        /// Paket URL'i (varsayılan: config.json pack_url)
        #[arg(long)]
        url: Option<String>,
    },
    /// Çeviri belleğini yedekle, ya da `backup list` ile yedekleri listele
    Backup {
        #[command(subcommand)]
        action: Option<BackupAction>,
    },
    /// Çeviri belleğini bir yedek dosyasından geri yükle (mevcut hali önce yedeklenir)
    Restore { file: PathBuf },
    /// Oyun üstü WuwaLang menüsü (F7)
    Ui,
    /// Sözlük yönetimi
    Glossary {
        #[command(subcommand)]
        action: GlossaryAction,
    },
}

#[derive(Subcommand)]
enum BackupAction {
    /// Yedekleri tarih, kayıt sayısı ve boyutla listele
    List,
}

#[derive(Subcommand)]
enum GlossaryAction {
    /// Terimleri listele
    List,
    /// Terim ekle veya değiştir
    Add {
        term: String,
        tr: String,
        /// Sonraki otomatik önerilerle değişmesin
        #[arg(long)]
        locked: bool,
    },
    /// Terimi kilitle
    Lock { term: String },
    /// Cümle gibi geçersiz ve kilitsiz girdileri sil (kilitliler kalır)
    Clean {
        /// Yalnızca say ve listele, dosyaya dokunma
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let gui = cli.command.is_none();
    if gui {
        detach_console();
    }
    match run(cli) {
        Ok(code) => code,
        Err(error) => {
            let message = format!("Hata: {error:#}");
            if gui {
                show_error(&message);
            } else {
                eprintln!("{message}");
            }
            ExitCode::from(1)
        }
    }
}

#[cfg(windows)]
fn detach_console() {
    // SAFETY: FreeConsole takes no arguments and only detaches this process from its console window.
    unsafe {
        windows_sys::Win32::System::Console::FreeConsole();
    }
}

#[cfg(not(windows))]
fn detach_console() {}

#[cfg(windows)]
fn show_error(message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let text: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
    let title: Vec<u16> = "WuwaLang".encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: both buffers are NUL-terminated UTF-16 and outlive the call.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn show_error(message: &str) {
    eprintln!("{message}");
}

fn run(cli: Cli) -> Result<ExitCode> {
    let mut cfg = load_config(cli.config.as_deref())?;
    if let Some(code) = &cli.lang {
        cfg.target = lang::validate(code)?.code.to_string();
    }
    match cli.command.unwrap_or(Command::Ui) {
        Command::Enable => {
            project(&cfg)?.enable()?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Disable => {
            project(&cfg)?.disable()?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Langs => cmd_langs(&cfg),
        Command::Setup { refresh_keys } => {
            prepare_tools(&config::tools_dir(), refresh_keys, &|text: &str| {
                println!("{text}")
            })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Extract { force } => {
            project(&cfg)?.extract(force)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Translate {
            engine,
            limit,
            dry_run,
        } => cmd_translate(cfg, engine, limit, dry_run),
        Command::Build => {
            auto_backup(&cfg);
            cmd_build(&cfg)
        }
        Command::Install => {
            auto_backup(&cfg);
            project(&cfg)?.install()?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Uninstall => {
            auto_backup(&cfg);
            project(&cfg)?.uninstall()?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Backup { action } => cmd_backup(&cfg, action),
        Command::Restore { file } => cmd_restore(&cfg, &file),
        Command::ImportPack { dir } => cmd_import_pack(&cfg, dir),
        Command::ExportPack { dir } => cmd_export_pack(&cfg, dir),
        Command::PullPack { url } => cmd_pull_pack(&cfg, url),
        Command::Ui => {
            wuwalang::ui::run()?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Glossary { action } => cmd_glossary(action, &cfg),
    }
}

fn load_config(path: Option<&Path>) -> Result<Config> {
    let path = path.map_or_else(config::default_path, Path::to_path_buf);
    let env: HashMap<String, String> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    config::load(&path, &env)
}

fn project(cfg: &Config) -> Result<Project> {
    Project::new(game_dir(cfg)?, project_dir(), config::tools_dir())
}

fn lang_of(cfg: &Config) -> Result<&'static Lang> {
    lang::validate(&cfg.target)
}

fn tm_path(lang: &Lang) -> PathBuf {
    lang::tm_path(&project_dir(), lang)
}

fn glossary_path(lang: &Lang) -> PathBuf {
    lang::glossary_path(&project_dir(), lang)
}

fn pack_dir(lang: &Lang) -> PathBuf {
    lang::pack_dir(&project_dir(), lang)
}

fn source_dir() -> PathBuf {
    project_dir().join("data").join("source")
}

/// Oyun kuruluysa veritabanlarını yeniler. Oyun yoksa önbellekteki kopya kullanılır.
fn ensure_source(project: &Project) -> Result<()> {
    if let Err(error) = project.extract(false) {
        if !source_dir().join("lang_multi_text.db").is_file() {
            return Err(error);
        }
        println!("Uyarı: {error:#} Önbellekteki kaynak veritabanları kullanılıyor.");
    }
    Ok(())
}

/// Önbellekteki kaynak veritabanlarından (dosya, tablo, kimlik, metin) satırlarını okur.
/// Boş metinler atlanır. Yalnızca Id ve Content sütunu olan tablolar okunur.
fn read_rows() -> Result<Vec<Row>> {
    let dir = source_dir();
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .with_context(|| format!("Kaynak veritabanı klasörü okunamadı: {}", dir.display()))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    paths.retain(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "db"));
    paths.sort();

    let mut rows = Vec::new();
    for path in paths {
        let file = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let tables: Vec<String> = db
            .prepare(
                "select name from sqlite_master where type='table' and name not like 'sqlite_%'",
            )?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for table in tables {
            let columns: Vec<String> = db
                .prepare(&format!("pragma table_info({})", quote_sql(&table)))?
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<_>>()?;
            if !(columns.iter().any(|c| c == "Id") && columns.iter().any(|c| c == "Content")) {
                continue;
            }
            let mut statement =
                db.prepare(&format!("select Id, Content from {}", quote_sql(&table)))?;
            let pairs = statement.query_map([], |row| {
                Ok((row.get::<_, SqlValue>(0)?, row.get::<_, SqlValue>(1)?))
            })?;
            for pair in pairs {
                let (identifier, source) = pair?;
                let SqlValue::Text(source) = source else {
                    continue;
                };
                if source.trim().is_empty() {
                    continue;
                }
                let identifier = match identifier {
                    SqlValue::Integer(number) => number.to_string(),
                    SqlValue::Text(text) => text,
                    _ => bail!("Desteklenmeyen Id türü: {file}/{table}"),
                };
                rows.push((file.clone(), table.clone(), identifier, source));
            }
        }
    }
    Ok(rows)
}

fn quote_sql(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn build_engine(cfg: &Config) -> Result<Box<dyn Engine>> {
    Ok(make_engine(cfg, Arc::new(UreqClient::new(ENGINE_TIMEOUT)))?)
}

/// Dry-run'da anahtar gerekmez: boş alanlar yer tutucu değerlerle doldurulup motor kurulur.
fn probe_engine(cfg: &Config, dry_run: bool) -> Result<Box<dyn Engine>> {
    if !dry_run {
        return build_engine(cfg);
    }
    fn fill(value: &mut String, placeholder: &str) {
        if value.is_empty() {
            *value = placeholder.to_string();
        }
    }
    let mut probe = cfg.clone();
    fill(&mut probe.yandex.api_key, "dry-run");
    fill(&mut probe.yandex.folder_id, "dry-run");
    fill(&mut probe.google.api_key, "dry-run");
    fill(&mut probe.openai.api_key, "dry-run");
    fill(&mut probe.openai.model, "dry-run");
    fill(&mut probe.openai.base_url, "https://example.invalid/v1");
    fill(&mut probe.anthropic.api_key, "dry-run");
    fill(&mut probe.anthropic.model, "dry-run");
    build_engine(&probe)
}

/// --limit: en fazla N benzersiz kaynak çevrilir.
fn trim_plan(mut plan: Plan, limit: usize) -> Plan {
    let mut kept: Vec<Batch> = Vec::new();
    let mut count = 0;
    for mut batch in plan.batches {
        if count >= limit {
            break;
        }
        if count + batch.items.len() > limit {
            let keep = limit - count;
            batch.items.truncate(keep);
            batch.sources.truncate(keep);
            batch.rids.truncate(keep);
            batch.maps.truncate(keep);
            batch.chars = batch
                .items
                .iter()
                .map(|item| item.text.chars().count())
                .sum();
        }
        count += batch.items.len();
        kept.push(batch);
    }
    plan.batches = kept;
    plan
}

fn cmd_translate(
    mut cfg: Config,
    engine: Option<String>,
    limit: usize,
    dry_run: bool,
) -> Result<ExitCode> {
    let lang = lang_of(&cfg)?;
    if let Some(engine) = engine {
        cfg.engine = engine;
    }
    let project = project(&cfg)?;
    ensure_source(&project)?;
    let records: Vec<Record> = read_rows()?
        .into_iter()
        .map(|(file, table, id, source)| Record {
            file,
            table,
            id,
            source,
        })
        .collect();
    let engine = probe_engine(&cfg, dry_run)?;
    let tm = TranslationMemory::open(&tm_path(lang))?;
    let mut glossary = Glossary::open(&glossary_path(lang))?;
    let mut plan = batching::plan(
        &records,
        &tm,
        &glossary,
        engine.as_ref(),
        cfg.batch.max_items,
        cfg.batch.max_chars,
    )?;
    if limit > 0 {
        plan = trim_plan(plan, limit);
    }
    if dry_run {
        let estimate = batching::dry_run(&plan, engine.as_ref());
        println!("{}", serde_json::to_string_pretty(&estimate)?);
        return Ok(ExitCode::SUCCESS);
    }
    let cancel = project.cancel_flag();
    let log = |message: &str| println!("{message}");
    let sleep = |duration: Duration| std::thread::sleep(duration);
    let outcome = batching::run(
        plan,
        engine.as_ref(),
        &tm,
        &mut glossary,
        &RunOptions {
            threads: cfg.batch.threads,
            cancelled: Some(cancel.as_ref()),
            log: &log,
            sleep: &sleep,
            attempts: RUN_ATTEMPTS,
        },
    );
    auto_backup(&cfg);
    let summary = outcome?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(if summary.failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    })
}

fn cmd_build(cfg: &Config) -> Result<ExitCode> {
    let lang = lang_of(cfg)?;
    let project = project(cfg)?;
    ensure_source(&project)?;
    let tm = TranslationMemory::open(&tm_path(lang))?;
    let rows = read_rows()?;
    let sources: Vec<&str> = rows.iter().map(|row| row.3.as_str()).collect();
    let hits = tm.get_many(&sources)?;
    let final_path = project.build(|_, _, _, source| hits.get(source).cloned())?;
    println!("Paket hazır: {}", final_path.display());
    Ok(ExitCode::SUCCESS)
}

fn cmd_import_pack(cfg: &Config, dir: Option<PathBuf>) -> Result<ExitCode> {
    let lang = lang_of(cfg)?;
    let project = project(cfg)?;
    ensure_source(&project)?;
    let rows = read_rows()?;
    let tm = TranslationMemory::open(&tm_path(lang))?;
    let stats = tm.import_pack(&dir.unwrap_or_else(|| pack_dir(lang)), Some(&rows))?;
    println!(
        "{{\"imported\": {}, \"skipped\": {}}}",
        stats.imported, stats.skipped
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_pull_pack(cfg: &Config, url: Option<String>) -> Result<ExitCode> {
    let lang = lang_of(cfg)?;
    let url = lang::pack_url_for(&url.unwrap_or_else(|| cfg.pack_url.clone()), lang);
    let url = url.trim();
    ensure!(
        !url.is_empty(),
        "Paket URL'i boş. --url ver ya da config.json pack_url alanını doldur."
    );
    let rows = if source_dir().join("lang_multi_text.db").is_file() {
        Some(read_rows()?)
    } else {
        None
    };
    let tm = TranslationMemory::open(&tm_path(lang))?;
    let log = |text: &str| println!("{text}");
    let stats = pull_pack_in(url, &tm, &lang::cache_dir(&project_dir(), lang), rows.as_deref(), &log)?;
    println!(
        "{} dosya tarandı, {} indirildi: {} satır alındı, {} atlandı.",
        stats.files, stats.downloaded, stats.imported, stats.skipped
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_export_pack(cfg: &Config, dir: Option<PathBuf>) -> Result<ExitCode> {
    let lang = lang_of(cfg)?;
    let project = project(cfg)?;
    ensure_source(&project)?;
    let rows = read_rows()?;
    let tm = TranslationMemory::open(&tm_path(lang))?;
    let count = tm.export_pack(&dir.unwrap_or_else(|| pack_dir(lang)), &rows)?;
    println!("{count} satır dışa aktarıldı.");
    Ok(ExitCode::SUCCESS)
}

fn cmd_glossary(action: GlossaryAction, cfg: &Config) -> Result<ExitCode> {
    let lang = lang_of(cfg)?;
    let mut glossary = Glossary::open(&glossary_path(lang))?;
    match action {
        GlossaryAction::List => {
            let mut entries: Vec<(&str, &Entry)> = glossary.iter().collect();
            entries.sort_by_key(|(term, _)| term.to_lowercase());
            for (term, entry) in &entries {
                let flag = if entry.locked { " [kilitli]" } else { "" };
                println!("{term}={}{flag}", entry.tr);
            }
            println!("Toplam: {}", glossary.len());
        }
        GlossaryAction::Add { term, tr, locked } => {
            glossary.add(&term, &tr, locked)?;
            println!("Eklendi: {term}={tr}");
        }
        GlossaryAction::Lock { term } => {
            glossary.lock(&term)?;
            println!("Kilitlendi: {term}");
        }
        GlossaryAction::Clean { dry_run } => {
            let report = glossary.clean(dry_run)?;
            let label = if dry_run { "Silinecek" } else { "Silindi" };
            for term in &report.removed {
                println!("{label}: {term}");
            }
            println!(
                "{} kayıt {}, {} kayıt kaldı, {} geçersiz ama kilitli kayıt korundu.",
                report.removed.len(),
                if dry_run { "silinecek" } else { "silindi" },
                report.kept,
                report.locked_kept
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Değişiklik varsa çeviri belleğinin yedeğini alır (ve kaynak varsa topluluk paketini).
/// Yedek hatası işi durdurmaz; yalnızca uyarı yazılır.
fn auto_backup(cfg: &Config) {
    let Ok(lang) = lang_of(cfg) else {
        return;
    };
    let rows = if source_dir().join("lang_multi_text.db").is_file() {
        read_rows().ok()
    } else {
        None
    };
    let dir = pack_dir(lang);
    let pack = rows.as_deref().map(|rows| (dir.as_path(), rows));
    match backup::auto(&project_dir(), lang.code, &tm_path(lang), pack) {
        Ok(Some(info)) => println!("Yedek alındı: {}", info.path.display()),
        Ok(None) => {}
        Err(error) => println!("Uyarı: yedek alınamadı: {error:#}"),
    }
}

fn cmd_backup(cfg: &Config, action: Option<BackupAction>) -> Result<ExitCode> {
    let lang = lang_of(cfg)?;
    let root = project_dir();
    match action {
        Some(BackupAction::List) => {
            let infos = backup::list(&root, lang.code)?;
            if infos.is_empty() {
                println!("Yedek yok.");
            }
            for info in &infos {
                let records = info.records.map_or("?".to_string(), |n| n.to_string());
                println!(
                    "{}  {:>8} kayıt  {:>10}  {}",
                    backup::display_stamp(&info.stamp),
                    records,
                    backup::format_bytes(info.bytes),
                    info.path.display()
                );
            }
        }
        None => match backup::create(&root, lang.code, &tm_path(lang), true)? {
            Some(info) => println!("Yedek alındı: {}", info.path.display()),
            None => println!("Yedeklenecek çeviri belleği yok."),
        },
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_restore(cfg: &Config, file: &Path) -> Result<ExitCode> {
    let lang = lang_of(cfg)?;
    match backup::restore(&project_dir(), lang.code, file, &tm_path(lang))? {
        Some(info) => println!(
            "Geri yüklendi. Önceki çeviri belleği yedeklendi: {}",
            info.path.display()
        ),
        None => println!("Geri yüklendi."),
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_langs(cfg: &Config) -> Result<ExitCode> {
    for lang in lang::LANGS {
        let mark = if lang.code.eq_ignore_ascii_case(&cfg.target) {
            "*"
        } else {
            " "
        };
        println!("{mark} {:<8} {:<22} {}", lang.code, lang.english, lang.native);
    }
    Ok(ExitCode::SUCCESS)
}
