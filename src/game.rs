use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context, Result};
use rusqlite::types::Value;
use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Value as Json};
use sha1::{Digest, Sha1};
use sha2::Sha256;

use crate::batching::Record;
use crate::config::Config;
use crate::net::{DownloadClient, Fetcher};
use crate::pak;

pub const GAME_EXE: &str = "Wuthering Waves.exe";
pub const SHIPPING_EXE: &str = "Client-Win64-Shipping.exe";
pub const PAK_NAME: &str = "pakchunk0-WuwaLang_900_P.pak";
pub const MOUNT_DIR: &str = "Lang_en/WuwaLang";
pub const MOUNT_PREFIX: &str = "Lang_en/WuwaLang/pakchunk0-WuwaLang_900_P";
/// Eski sürümün (WuWa-TR) kurulum dizini ve mount adı. Kurma, kaldırma ve
/// etkinleştirme bunları da tanır; kurulumda temizlenir.
const LEGACY_PAK_NAME: &str = "pakchunk0-WuwaRu_900_P.pak";
const LEGACY_MOUNT_DIR: &str = "Lang_en/WuwaTR";
const LEGACY_MOUNT_PREFIX: &str = "Lang_en/WuwaTR/pakchunk0-WuwaRu_900_P";
const MOUNT_PRIORITY: u32 = 900;
const DB_SUBDIR: &str = "Client/Content/Aki/ConfigDB/en";
const EXTRACT_PREFIX: &str = "ConfigDB/en/lang_";
const FMODEL_SHA256: &str = "86426493c63f0af2ccf6d1d8b498a737723069f6c25a573a46e94d1a3f26e835";
const FMODEL_URL: &str =
    "https://github.com/Herselfta/FModelCLI/releases/download/v1.0.2/FModelCLI.exe";
const KEYS_URL: &str = "https://raw.githubusercontent.com/yarik0chka/wuwa-keys/main/keys.json";
const BOM: &[u8] = b"\xef\xbb\xbf";
const COMMON_GAME_DIRS: &[&str] = &[
    r"D:\Games\steamapps\common\Wuthering Waves",
    r"C:\Program Files (x86)\Steam\steamapps\common\Wuthering Waves",
    r"C:\Program Files\Steam\steamapps\common\Wuthering Waves",
    r"C:\Wuthering Waves\Wuthering Waves Game",
    r"C:\Program Files\Wuthering Waves\Wuthering Waves Game",
];

pub fn find_game(configured: Option<&Path>) -> Option<PathBuf> {
    configured
        .into_iter()
        .map(Path::to_path_buf)
        .chain(COMMON_GAME_DIRS.iter().map(PathBuf::from))
        .find(|dir| dir.join(GAME_EXE).is_file())
}

pub fn root_from_exe(exe: &Path) -> Result<PathBuf> {
    let name = exe.file_name().and_then(OsStr::to_str).unwrap_or_default();
    ensure!(
        name.eq_ignore_ascii_case(GAME_EXE) || name.eq_ignore_ascii_case(SHIPPING_EXE),
        "Seçilen dosya oyunun exe'si değil: {name}. '{GAME_EXE}' ya da '{SHIPPING_EXE}' seç."
    );
    exe.ancestors()
        .skip(1)
        .find(|dir| {
            dir.join("Client").is_dir()
                && (dir.join("Engine").is_dir() || dir.join("Client/Saved/Resources").is_dir())
        })
        .map(Path::to_path_buf)
        .with_context(|| {
            format!(
                "'Wuthering Waves Game' klasörü bulunamadı ({}). Oyunun kurulu klasöründeki exe'yi seç.",
                exe.display()
            )
        })
}

pub fn game_dir(cfg: &Config) -> Result<PathBuf> {
    let exe = cfg.game_exe.trim();
    if exe.is_empty() {
        return Ok(PathBuf::from(&cfg.game_path));
    }
    root_from_exe(Path::new(exe))
}

fn version_key(name: &str) -> Option<Vec<u64>> {
    let parts: Vec<u64> = name
        .split('.')
        .map(|part| {
            (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
                .then(|| part.parse::<u64>().ok())
                .flatten()
        })
        .collect::<Option<_>>()?;
    (parts.len() >= 2).then_some(parts)
}

pub fn resource_root(game: &Path) -> Result<PathBuf> {
    let root = game.join("Client/Saved/Resources");
    let mut best: Option<(Vec<u64>, PathBuf)> = None;
    for entry in fs::read_dir(&root)
        .with_context(|| format!("Kaynak klasörü okunamadı: {}", root.display()))?
    {
        let path = entry?.path();
        let Some(key) = path
            .file_name()
            .and_then(OsStr::to_str)
            .and_then(version_key)
        else {
            continue;
        };
        if path.join("Mount/MountResource.txt").exists()
            && best.as_ref().is_none_or(|(known, _)| key > *known)
        {
            best = Some((key, path));
        }
    }
    best.map(|(_, path)| path)
        .context("Etkin oyun kaynak sürümü bulunamadı. Resmi başlatıcıyı açıp güncellemeyi bitir.")
}

#[cfg(windows)]
pub fn game_running() -> bool {
    const CLIENT_EXE: &str = "Client-Win64-Shipping.exe";
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    // SAFETY: snapshot handle is checked, the entry struct is initialized with its size, and the handle is closed.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        let mut more = Process32FirstW(snapshot, &mut entry) != 0;
        while more {
            let end = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
            if name.eq_ignore_ascii_case(CLIENT_EXE) {
                found = true;
                break;
            }
            more = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        found
    }
}

#[cfg(not(windows))]
pub fn game_running() -> bool {
    false
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha1_file(path: &Path) -> Result<String> {
    let mut hasher = Sha1::new();
    let mut file =
        File::open(path).with_context(|| format!("Dosya açılamadı: {}", path.display()))?;
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_lower(&hasher.finalize()).to_uppercase())
}

fn sha256_hex(data: &[u8]) -> String {
    hex_lower(&Sha256::digest(data))
}

pub fn verify_sha256(data: &[u8], expected: &str) -> Result<()> {
    ensure!(
        sha256_hex(data) == expected,
        "SHA-256 doğrulaması başarısız."
    );
    Ok(())
}

/// Bu aracın yazdığı mount satırı mı? Eski sürümün satırları da sayılır.
fn is_own_line(line: &str) -> bool {
    let name = line
        .split(',')
        .next()
        .unwrap_or("")
        .replace('\\', "/")
        .to_lowercase();
    name == MOUNT_PREFIX.to_lowercase() || name == LEGACY_MOUNT_PREFIX.to_lowercase()
}

pub fn update_mount(raw: &[u8], entry: Option<&str>) -> Result<Vec<u8>> {
    let (bom, body) = match raw.strip_prefix(BOM) {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    let text = std::str::from_utf8(body).context("MountLang_en.txt UTF-8 değil.")?;
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<&str> = text.lines().filter(|line| !is_own_line(line)).collect();
    if let Some(entry) = entry {
        let mount = lines
            .iter()
            .position(|line| *line == "::Mount::")
            .context("MountLang_en.txt biçimi tanınmadı.")?;
        ensure!(
            lines.contains(&"::Del::"),
            "MountLang_en.txt biçimi tanınmadı."
        );
        let start = mount + 1;
        let end = lines[start..]
            .iter()
            .position(|line| line.starts_with("::"))
            .map_or(lines.len(), |i| start + i);
        lines.insert(end, entry);
    }
    let mut out = Vec::with_capacity(raw.len() + 128);
    if bom {
        out.extend_from_slice(BOM);
    }
    out.extend_from_slice(lines.join(newline).as_bytes());
    out.extend_from_slice(newline.as_bytes());
    Ok(out)
}

fn mount_has_entry(raw: &[u8]) -> bool {
    let text = String::from_utf8_lossy(raw.strip_prefix(BOM).unwrap_or(raw));
    text.lines().any(is_own_line)
}

/// Kurulu paketin dosyası: yeni dizinde, yoksa eski sürümün dizininde.
fn installed_pak(resources: &Path) -> Option<PathBuf> {
    [(MOUNT_DIR, PAK_NAME), (LEGACY_MOUNT_DIR, LEGACY_PAK_NAME)]
        .iter()
        .map(|(dir, name)| resources.join(dir).join(name))
        .find(|path| path.is_file())
}

/// Eski sürümün paketini yeni dizine taşır. Eski paket yoksa hiçbir şey yapmaz.
fn move_legacy_pak(resources: &Path) -> Result<()> {
    let old = resources.join(LEGACY_MOUNT_DIR).join(LEGACY_PAK_NAME);
    if !old.is_file() {
        return Ok(());
    }
    let dir = resources.join(MOUNT_DIR);
    fs::create_dir_all(&dir)?;
    fs::rename(&old, dir.join(PAK_NAME))?;
    let _ = fs::remove_dir(resources.join(LEGACY_MOUNT_DIR));
    Ok(())
}

/// Eski sürümün paketini ve boş kalan dizinini siler.
fn remove_legacy_pak(resources: &Path) -> Result<()> {
    let old = resources.join(LEGACY_MOUNT_DIR).join(LEGACY_PAK_NAME);
    if old.is_file() {
        fs::remove_file(&old)?;
    }
    let _ = fs::remove_dir(resources.join(LEGACY_MOUNT_DIR));
    Ok(())
}

pub fn is_installed(game: &Path) -> bool {
    let Ok(resources) = resource_root(game) else {
        return false;
    };
    let Ok(mount) = fs::read(resources.join("Mount/MountLang_en.txt")) else {
        return false;
    };
    installed_pak(&resources).is_some() && mount_has_entry(&mount)
}

pub fn is_enabled(game: &Path) -> bool {
    let Ok(resources) = resource_root(game) else {
        return false;
    };
    fs::read(resources.join("Mount/MountLang_en.txt"))
        .is_ok_and(|mount| mount_has_entry(&mount))
}

fn quote_sql(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn text_id(value: &Value) -> Result<String> {
    match value {
        Value::Integer(number) => Ok(number.to_string()),
        Value::Text(text) => Ok(text.clone()),
        _ => bail!("Desteklenmeyen Id türü"),
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn reset_dir(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    fs::create_dir_all(path)?;
    Ok(())
}

fn write_json_atomic(path: &Path, value: &Json) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    let mut file = File::create(&tmp)?;
    std::io::Write::write_all(&mut file, &serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    Ok(())
}

fn read_json(path: &Path) -> Result<Json> {
    serde_json::from_slice(
        &fs::read(path).with_context(|| format!("Okunamadı: {}", path.display()))?,
    )
    .with_context(|| format!("JSON çözümlenemedi: {}", path.display()))
}

fn json_str<'a>(value: &'a Json, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Json::as_str)
        .with_context(|| format!("Eksik alan: {key}"))
}

fn signature(paks: &[PathBuf]) -> Result<String> {
    let mut entries = Vec::with_capacity(paks.len());
    for path in paks {
        let meta = fs::metadata(path)?;
        let mtime = meta.modified()?.duration_since(UNIX_EPOCH)?.as_nanos() as u64;
        entries.push(json!([path.to_string_lossy(), meta.len(), mtime]));
    }
    Ok(sha256_hex(&serde_json::to_vec(&entries)?))
}

fn parse_keys(response: &Json) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    keys.push(
        response
            .get("mainKey")
            .and_then(Json::as_str)
            .context("Invalid resource key format.")?
            .to_owned(),
    );
    for item in response
        .get("dynamicKeys")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
    {
        keys.push(
            item.get("key")
                .and_then(Json::as_str)
                .context("Invalid resource key format.")?
                .to_owned(),
        );
    }
    let mut unique: Vec<String> = Vec::new();
    for key in keys {
        if !unique.contains(&key) {
            unique.push(key);
        }
    }
    ensure!(!unique.is_empty(), "No resource keys returned.");
    for key in &unique {
        let valid = key.len() == 66
            && key.starts_with("0x")
            && key[2..].bytes().all(|b| b.is_ascii_hexdigit());
        ensure!(valid, "Invalid resource key format.");
    }
    Ok(unique)
}

pub fn prepare_tools(tools: &Path, refresh_keys: bool, log: &dyn Fn(&str)) -> Result<()> {
    let client = DownloadClient::new();
    let sleep = |wait: Duration| thread::sleep(wait);
    let fetcher = Fetcher {
        client: &client,
        log,
        sleep: &sleep,
    };
    let extractor_dir = tools.join("fmodel");
    fs::create_dir_all(&extractor_dir)?;
    let extractor = extractor_dir.join("FModelCLI.exe");
    if extractor.is_file() {
        ensure!(
            sha256_hex(&fs::read(&extractor)?) == FMODEL_SHA256,
            "Existing FModelCLI.exe does not match the pinned release. Move it aside and retry."
        );
    } else {
        let bytes = fetcher.get(FMODEL_URL)?;
        let part = extractor_dir.join("FModelCLI.exe.part");
        fs::write(&part, &bytes)?;
        if let Err(error) = verify_sha256(&fs::read(&part)?, FMODEL_SHA256) {
            let _ = fs::remove_file(&part);
            return Err(error.context(format!("{FMODEL_URL} SHA-256 doğrulaması başarısız.")));
        }
        fs::rename(&part, &extractor)?;
    }
    log("FModelCLI v1.0.2: SHA-256 doğrulandı.");

    let keyfile = tools.join("pakkeys.txt");
    if refresh_keys || !keyfile.is_file() {
        let response: Json = serde_json::from_slice(&fetcher.get(KEYS_URL)?)
            .with_context(|| format!("{KEYS_URL} JSON çözümlenemedi."))?;
        let keys = parse_keys(&response)?;
        let mut text = String::new();
        for key in &keys {
            text.push_str(key);
            text.push_str("\r\n");
        }
        let part = tools.join("pakkeys.txt.part");
        fs::write(&part, text)?;
        fs::rename(&part, &keyfile)?;
        log(&format!("Kaynak anahtarları hazırlandı: {}.", keys.len()));
    } else {
        log("Mevcut kaynak anahtarları korundu. Oyun güncellemesinden sonra yenile.");
    }
    Ok(())
}

pub struct Project {
    pub game: PathBuf,
    home: PathBuf,
    tools: PathBuf,
    cancelled: Arc<AtomicBool>,
    log: Box<dyn Fn(&str) + Send + Sync>,
}

impl Project {
    pub fn new(game: PathBuf, home: PathBuf, tools: PathBuf) -> Result<Self> {
        let home = std::path::absolute(home)?;
        fs::create_dir_all(home.join("data"))?;
        Ok(Self {
            game,
            home,
            tools,
            cancelled: Arc::new(AtomicBool::new(false)),
            log: Box::new(|message| println!("{message}")),
        })
    }

    pub fn with_log(mut self, log: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.log = Box::new(log);
        self
    }

    pub fn with_cancel(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancelled = flag;
        self
    }

    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    fn data(&self) -> PathBuf {
        self.home.join("data")
    }

    fn source(&self) -> PathBuf {
        self.data().join("source")
    }

    pub fn source_ready(&self) -> bool {
        self.source().join("lang_multi_text.db").is_file()
    }

    pub fn rows(&self) -> Result<Vec<Record>> {
        let mut records = Vec::new();
        for path in list_dbs(&self.source())? {
            let file = path
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_owned();
            let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let tables: Vec<String> = conn
                .prepare("select name from sqlite_master where type='table' and name not like 'sqlite_%'")?
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for table in tables {
                let columns: Vec<String> = conn
                    .prepare(&format!("pragma table_info({})", quote_sql(&table)))?
                    .query_map([], |row| row.get::<_, String>(1))?
                    .collect::<rusqlite::Result<_>>()?;
                if !(columns.iter().any(|c| c == "Id") && columns.iter().any(|c| c == "Content")) {
                    continue;
                }
                let rows: Vec<(Value, Value)> = conn
                    .prepare(&format!("select Id, Content from {}", quote_sql(&table)))?
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?;
                for (identifier, content) in rows {
                    let Value::Text(source) = content else {
                        continue;
                    };
                    if source.trim().is_empty() {
                        continue;
                    }
                    records.push(Record {
                        file: file.clone(),
                        table: table.clone(),
                        id: text_id(&identifier)?,
                        source,
                    });
                }
            }
        }
        Ok(records)
    }

    fn output(&self) -> PathBuf {
        self.home.join("output")
    }

    fn extractor(&self) -> PathBuf {
        self.tools.join("fmodel/FModelCLI.exe")
    }

    fn key_argument(&self) -> Result<std::ffi::OsString> {
        let keys = std::path::absolute(self.tools.join("pakkeys.txt"))?;
        Ok(format!("@{}", keys.display()).into())
    }

    fn run_tool<'a>(
        &self,
        program: &OsStr,
        args: impl IntoIterator<Item = &'a OsStr>,
        log_path: &Path,
    ) -> Result<()> {
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let log = File::create(log_path)?;
        let log_err = log.try_clone()?;
        let mut command = Command::new(program);
        command.args(args).stdout(log).stderr(log_err);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("Çalıştırılamadı: {}", Path::new(program).display()))?;
        loop {
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    bail!(
                        "{} hatası. Ayrıntılar: {}",
                        Path::new(program)
                            .file_name()
                            .unwrap_or(program)
                            .to_string_lossy(),
                        log_path.display()
                    );
                }
                return Ok(());
            }
            if self.cancelled.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                bail!("İşlem iptal edildi.");
            }
            thread::sleep(Duration::from_millis(200));
        }
    }

    pub fn validate(&self) -> Result<PathBuf> {
        ensure!(
            self.game.join(GAME_EXE).is_file(),
            "Wuthering Waves.exe bulunamadı. Oyun yolunu kontrol et."
        );
        resource_root(&self.game)
    }

    fn official_paks(&self, resources: &Path) -> Result<Vec<PathBuf>> {
        let mut result: Vec<PathBuf> = Vec::new();
        for entry in fs::read_dir(self.game.join("Client/Content/Paks"))? {
            let path = entry?.path();
            if path.is_file() && path.extension().is_some_and(|ext| ext == "pak") {
                result.push(path);
            }
        }
        for mount in ["MountResource.txt", "MountLauncher.txt"] {
            let raw = fs::read(resources.join("Mount").join(mount))?;
            let body: &[u8] = raw.strip_prefix(BOM).unwrap_or(&raw);
            let text = std::str::from_utf8(body)?;
            let mut section = false;
            for line in text.lines() {
                if line == "::Mount::" {
                    section = true;
                    continue;
                }
                if line.starts_with("::") {
                    section = false;
                    continue;
                }
                if section && line.contains(',') {
                    let name = line.split(',').next().unwrap_or("");
                    ensure!(
                        Path::new(name)
                            .components()
                            .all(|c| matches!(c, std::path::Component::Normal(_))),
                        "Oyun manifestinde geçersiz yol var."
                    );
                    let path = resources.join(format!("{name}.pak"));
                    ensure!(path.is_file(), "Eksik oyun paketi: {}", path.display());
                    result.push(path);
                }
            }
        }
        ensure!(!result.is_empty(), "Oyun paketleri bulunamadı.");
        Ok(result)
    }

    pub fn extract(&self, force: bool) -> Result<PathBuf> {
        if !(self.extractor().is_file() && self.tools.join("pakkeys.txt").is_file()) {
            prepare_tools(&self.tools, false, &*self.log)?;
        }
        let resources = self.validate()?;
        let paks = self.official_paks(&resources)?;
        let current = signature(&paks)?;
        let stamp = self.data().join("source-manifest.json");
        let source = self.source();
        if !force
            && stamp.exists()
            && read_json(&stamp).is_ok_and(|m| m.get("signature").and_then(Json::as_str) == Some(current.as_str()))
            && source.join("lang_multi_text.db").is_file()
        {
            (self.log)("Metin sürümü değişmedi; yerel veritabanları kullanılıyor.");
            return Ok(resources);
        }
        (self.log)("Etkin paketlerden İngilizce veritabanları çıkarılıyor…");
        ensure!(
            self.extractor().is_file() && self.tools.join("pakkeys.txt").is_file(),
            "tools/fmodel/FModelCLI.exe veya tools/pakkeys.txt eksik. Önce `wuwalang setup` çalıştır."
        );
        let stage = self.data().join("pak-input");
        reset_dir(&stage)?;
        self.stage_paks(&paks, &stage)?;
        let raw = self.data().join("extract-tmp");
        reset_dir(&raw)?;
        let key_argument = self.key_argument()?;
        self.run_tool(
            self.extractor().as_os_str(),
            [
                stage.as_os_str(),
                key_argument.as_os_str(),
                raw.as_os_str(),
                OsStr::new(EXTRACT_PREFIX),
            ],
            &self.data().join("extract.log"),
        )?;
        let extracted = raw.join(DB_SUBDIR);
        ensure!(
            extracted.join("lang_multi_text.db").is_file(),
            "Veritabanları çıkarılamadı. Güncel paket anahtarı/çıkarıcı gerekli; data/extract.log dosyasına bak."
        );
        for path in list_dbs(&extracted)? {
            let conn = Connection::open(&path)?;
            let check: String = conn.query_row("pragma quick_check", [], |row| row.get(0))?;
            ensure!(check == "ok", "Bozuk kaynak veritabanı: {}", path.display());
        }
        let fresh = self.data().join("source-new");
        if fresh.exists() {
            fs::remove_dir_all(&fresh)?;
        }
        copy_tree(&extracted, &fresh)?;
        let source_old = self.data().join("source-old");
        if source_old.exists() {
            fs::remove_dir_all(&source_old)?;
        }
        let had_source = source.exists();
        if had_source {
            fs::rename(&source, &source_old)?;
        }
        if let Err(error) = fs::rename(&fresh, &source) {
            if had_source {
                let _ = fs::rename(&source_old, &source);
            }
            return Err(error.into());
        }
        if had_source {
            fs::remove_dir_all(&source_old)?;
        }
        let manifest = json!({
            "signature": current,
            "resource_version": resources.file_name().and_then(OsStr::to_str).unwrap_or_default(),
            "files": paks.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
        });
        write_json_atomic(&stamp, &manifest)?;
        (self.log)(&format!(
            "{} veritabanı çıkarıldı.",
            list_dbs(&source)?.len()
        ));
        Ok(resources)
    }

    #[cfg(windows)]
    fn stage_paks(&self, paks: &[PathBuf], stage: &Path) -> Result<()> {
        let mut parents: Vec<&Path> = Vec::new();
        for pak in paks {
            if let Some(parent) = pak.parent() {
                if !parents.contains(&parent) {
                    parents.push(parent);
                }
            }
        }
        let literal =
            |value: &Path| format!("'{}'", value.display().to_string().replace('\'', "''"));
        for (i, parent) in parents.iter().enumerate() {
            let target = std::path::absolute(stage.join(format!("{i:03}")))?;
            let script = format!(
                "New-Item -ItemType Junction -Path {} -Target {} -ErrorAction Stop | Out-Null",
                literal(&target),
                literal(&std::path::absolute(parent)?)
            );
            self.run_tool(
                OsStr::new("powershell.exe"),
                [
                    OsStr::new("-NoProfile"),
                    OsStr::new("-NonInteractive"),
                    OsStr::new("-Command"),
                    OsStr::new(script.as_str()),
                ],
                &self.data().join("junction.log"),
            )?;
        }
        Ok(())
    }

    #[cfg(not(windows))]
    fn stage_paks(&self, paks: &[PathBuf], stage: &Path) -> Result<()> {
        for (i, pak) in paks.iter().enumerate() {
            let dir = stage.join(format!("{i:03}"));
            fs::create_dir_all(&dir)?;
            let name = pak.file_name().context("Geçersiz paket adı")?;
            std::os::unix::fs::symlink(fs::canonicalize(pak)?, dir.join(name))?;
        }
        Ok(())
    }

    pub fn build<F>(&self, mut lookup: F) -> Result<PathBuf>
    where
        F: FnMut(&str, &str, &str, &str) -> Option<String>,
    {
        let resources = self.extract(false)?;
        let stage = self.data().join("package-stage");
        reset_dir(&stage)?;
        let target = stage.join(DB_SUBDIR);
        copy_tree(&self.source(), &target)?;
        let mut translated = 0u64;
        let mut total = 0u64;
        let mut changed = 0u64;
        let mut pending: HashSet<String> = HashSet::new();
        let dbs = list_dbs(&target)?;
        for path in &dbs {
            if self.cancelled.load(Ordering::Relaxed) {
                bail!("İşlem iptal edildi.");
            }
            let file_name = path
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_owned();
            let mut conn = Connection::open(path)?;
            {
                let tx = conn.transaction()?;
                let tables: Vec<String> = tx
                    .prepare("select name from sqlite_master where type='table' and name not like 'sqlite_%'")?
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                for table in tables {
                    let columns: Vec<String> = tx
                        .prepare(&format!("pragma table_info({})", quote_sql(&table)))?
                        .query_map([], |row| row.get::<_, String>(1))?
                        .collect::<rusqlite::Result<_>>()?;
                    if !(columns.iter().any(|c| c == "Id")
                        && columns.iter().any(|c| c == "Content"))
                    {
                        continue;
                    }
                    let rows: Vec<(Value, Value)> = tx
                        .prepare(&format!("select Id, Content from {}", quote_sql(&table)))?
                        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                        .collect::<rusqlite::Result<_>>()?;
                    let mut updates: Vec<(String, Value)> = Vec::new();
                    for (identifier, content) in rows {
                        let Value::Text(source) = content else {
                            continue;
                        };
                        if source.trim().is_empty() {
                            continue;
                        }
                        total += 1;
                        let id_text = text_id(&identifier)?;
                        match lookup(&file_name, &table, &id_text, &source) {
                            None => {
                                pending.insert(source);
                            }
                            Some(turkish) => {
                                translated += 1;
                                if turkish != source {
                                    updates.push((turkish, identifier));
                                    changed += 1;
                                }
                            }
                        }
                    }
                    let sql = format!("update {} set Content=? where Id=?", quote_sql(&table));
                    for (turkish, identifier) in &updates {
                        tx.execute(&sql, params![turkish, identifier])?;
                    }
                }
                tx.commit()?;
            }
            let check: String = conn.query_row("pragma quick_check", [], |row| row.get(0))?;
            ensure!(
                check == "ok",
                "Derlenen veritabanı doğrulanamadı: {file_name}"
            );
        }

        let output = self.output();
        fs::create_dir_all(&output)?;
        let tmp = output.join("translation.tmp.pak");
        pak::pack(&stage, &tmp, Some(self.cancelled.as_ref()))?;

        let check = self.data().join("package-check");
        if check.exists() {
            fs::remove_dir_all(&check)?;
        }
        let check_input = self.data().join("package-input");
        reset_dir(&check_input)?;
        fs::copy(&tmp, check_input.join(PAK_NAME))?;
        let key_argument = self.key_argument()?;
        self.run_tool(
            self.extractor().as_os_str(),
            [
                check_input.as_os_str(),
                key_argument.as_os_str(),
                check.as_os_str(),
                OsStr::new(EXTRACT_PREFIX),
            ],
            &self.data().join("package-check.log"),
        )?;
        for path in &dbs {
            let name = path.file_name().context("Geçersiz veritabanı adı")?;
            let other = check.join(DB_SUBDIR).join(name);
            ensure!(
                other.is_file() && sha1_file(path)? == sha1_file(&other)?,
                "Paket doğrulaması başarısız: {}",
                name.to_string_lossy()
            );
        }

        let final_path = output.join(PAK_NAME);
        fs::rename(&tmp, &final_path)?;
        let source_signature = json_str(
            &read_json(&self.data().join("source-manifest.json"))?,
            "signature",
        )?
        .to_owned();
        let meta = json!({
            "created": SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            "resource_version": resources.file_name().and_then(OsStr::to_str).unwrap_or_default(),
            "source_signature": source_signature,
            "databases": dbs.len(),
            "rows": total,
            "translated_rows": translated,
            "changed_rows": changed,
            "pending_unique_strings": pending.len(),
            "complete": pending.is_empty(),
            "sha1": sha1_file(&final_path)?,
            "game_verified": false,
            "pak_version": "WuWa-V12",
        });
        write_json_atomic(&output.join("manifest.json"), &meta)?;
        (self.log)(&format!(
            "Paket derlendi: {translated}/{total} satır çevrili; çevrilmemiş benzersiz metin: {}.",
            pending.len()
        ));
        Ok(final_path)
    }

    pub fn install(&self) -> Result<()> {
        let resources = self.extract(false)?;
        ensure!(!game_running(), "Çeviriyi kurmadan önce oyunu kapat.");
        let output = self.output().join(PAK_NAME);
        let manifest = read_json(&self.output().join("manifest.json"))?;
        let current = read_json(&self.data().join("source-manifest.json"))?;
        ensure!(
            json_str(&manifest, "resource_version")?
                == resources
                    .file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or_default()
                && json_str(&manifest, "source_signature")? == json_str(&current, "signature")?,
            "Paket eski. Önce çeviriyi yeniden derle."
        );
        ensure!(
            sha1_file(&output)? == json_str(&manifest, "sha1")?,
            "Paket derlemeden sonra değiştirilmiş."
        );
        let directory = resources.join(MOUNT_DIR);
        fs::create_dir_all(&directory)?;
        let dest = directory.join(PAK_NAME);
        let temp = dest.with_extension("tmp");
        fs::copy(&output, &temp)?;
        fs::rename(&temp, &dest)?;

        let mount = resources.join("Mount/MountLang_en.txt");
        let original = fs::read(&mount)?;
        let backup = self
            .data()
            .join("backups")
            .join(resources.file_name().unwrap_or_default());
        fs::create_dir_all(&backup)?;
        fs::write(
            backup.join(format!("{}-MountLang_en.txt", time_ns())),
            &original,
        )?;
        let entry = format!("{MOUNT_PREFIX},{MOUNT_PRIORITY},{},,,", sha1_file(&dest)?);
        let changed = update_mount(&original, Some(&entry))?;
        let temp_mount = mount.with_extension("tmp");
        fs::write(&temp_mount, changed)?;
        fs::rename(&temp_mount, &mount)?;
        remove_legacy_pak(&resources)?;

        write_json_atomic(
            &self.data().join("installed.json"),
            &json!({
                "resources": resources.to_string_lossy(),
                "path": dest.to_string_lossy(),
                "installed": SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            }),
        )?;
        (self.log)("Paket İngilizce dile bağlandı. Oyunda English seç.");
        Ok(())
    }

    pub fn uninstall(&self) -> Result<()> {
        ensure!(!game_running(), "Çeviriyi kaldırmadan önce oyunu kapat.");
        let root = self.game.join("Client/Saved/Resources");
        if !root.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(&root)? {
            let resources = entry?.path();
            let mount = resources.join("Mount/MountLang_en.txt");
            if !mount.is_file() {
                continue;
            }
            let old = fs::read(&mount)?;
            let new = update_mount(&old, None)?;
            if old != new {
                let backup = self
                    .data()
                    .join("backups")
                    .join(resources.file_name().unwrap_or_default());
                fs::create_dir_all(&backup)?;
                fs::write(
                    backup.join(format!("{}-uninstall-MountLang_en.txt", time_ns())),
                    &old,
                )?;
                let temp = mount.with_extension("tmp");
                fs::write(&temp, &new)?;
                fs::rename(&temp, &mount)?;
            }
            let pak = resources.join(MOUNT_DIR).join(PAK_NAME);
            if pak.exists() {
                fs::remove_file(&pak)?;
            }
            let _ = fs::remove_dir(resources.join(MOUNT_DIR));
            remove_legacy_pak(&resources)?;
        }
        (self.log)("Kendi paket devre dışı bırakıldı. İngilizce dil geri yüklendi.");
        Ok(())
    }

    fn rewrite_mount(&self, resources: &Path, label: &str, entry: Option<&str>) -> Result<()> {
        let mount = resources.join("Mount/MountLang_en.txt");
        let original = fs::read(&mount)?;
        let changed = update_mount(&original, entry)?;
        if changed == original {
            return Ok(());
        }
        let backup = self
            .data()
            .join("backups")
            .join(resources.file_name().unwrap_or_default());
        fs::create_dir_all(&backup)?;
        fs::write(
            backup.join(format!("{}-{label}-MountLang_en.txt", time_ns())),
            &original,
        )?;
        let temp = mount.with_extension("tmp");
        fs::write(&temp, &changed)?;
        fs::rename(&temp, &mount)?;
        Ok(())
    }

    pub fn enable(&self) -> Result<()> {
        ensure!(!game_running(), "Çeviriyi açmadan önce oyunu kapat.");
        let resources = self.validate()?;
        move_legacy_pak(&resources)?;
        let pak = resources.join(MOUNT_DIR).join(PAK_NAME);
        ensure!(
            pak.is_file(),
            "Türkçe paket kurulu değil. Önce `install` çalıştır."
        );
        let entry = format!("{MOUNT_PREFIX},{MOUNT_PRIORITY},{},,,", sha1_file(&pak)?);
        self.rewrite_mount(&resources, "enable", Some(&entry))?;
        (self.log)("Türkçe paket etkinleştirildi. Oyunda English seç.");
        Ok(())
    }

    pub fn disable(&self) -> Result<()> {
        ensure!(
            !game_running(),
            "Çeviriyi devre dışı bırakmadan önce oyunu kapat."
        );
        let resources = self.validate()?;
        self.rewrite_mount(&resources, "disable", None)?;
        (self.log)("Türkçe paket devre dışı bırakıldı. Paket dosyası yerinde duruyor.");
        Ok(())
    }
}

fn time_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn list_dbs(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut dbs: Vec<PathBuf> = fs::read_dir(dir)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    dbs.retain(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "db"));
    dbs.sort();
    Ok(dbs)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNT_SAMPLE: &str = "::Mount::\r\nOther/Pak,100,ABCDEF,,,\r\n::Del::\r\n";

    #[test]
    fn mount_entry_is_added_before_next_section_and_removed() {
        let added = update_mount(
            MOUNT_SAMPLE.as_bytes(),
            Some(&format!("{MOUNT_PREFIX},900,AB,,,")),
        )
        .unwrap();
        let text = String::from_utf8(added.clone()).unwrap();
        assert_eq!(
            text,
            format!(
                "::Mount::\r\nOther/Pak,100,ABCDEF,,,\r\n{MOUNT_PREFIX},900,AB,,,\r\n::Del::\r\n"
            )
        );
        assert!(mount_has_entry(&added));
        let removed = update_mount(&added, None).unwrap();
        assert_eq!(removed, MOUNT_SAMPLE.as_bytes());
        assert!(!mount_has_entry(&removed));
    }

    #[test]
    fn mount_keeps_bom_and_lf_newlines() {
        let mut raw = BOM.to_vec();
        raw.extend_from_slice(b"::Mount::\nX,1,2,,,\n::Del::\n");
        let out = update_mount(&raw, Some("E,900,AB,,,")).unwrap();
        assert!(out.starts_with(BOM));
        assert_eq!(
            String::from_utf8(out[3..].to_vec()).unwrap(),
            "::Mount::\nX,1,2,,,\nE,900,AB,,,\n::Del::\n"
        );
    }

    #[test]
    fn mount_rejects_unknown_format() {
        assert!(update_mount(b"garbage\n", Some("E,900,AB,,,")).is_err());
        assert!(update_mount(b"::Mount::\n", Some("E,900,AB,,,")).is_err());
    }

    fn fake_game(root: &Path, version: &str) -> PathBuf {
        let game = root.join("game");
        let resources = game.join("Client/Saved/Resources").join(version);
        fs::create_dir_all(resources.join("Mount")).unwrap();
        fs::write(resources.join("Mount/MountResource.txt"), "").unwrap();
        fs::write(resources.join("Mount/MountLang_en.txt"), MOUNT_SAMPLE).unwrap();
        game
    }

    #[test]
    fn is_installed_follows_pak_and_mount_entry() {
        let dir = tempfile::tempdir().unwrap();
        let game = fake_game(dir.path(), "1.2.3");
        let resources = game.join("Client/Saved/Resources/1.2.3");
        assert!(!is_installed(&game));

        fs::create_dir_all(resources.join(MOUNT_DIR)).unwrap();
        fs::write(resources.join(MOUNT_DIR).join(PAK_NAME), b"pak").unwrap();
        assert!(!is_installed(&game));

        let mount = resources.join("Mount/MountLang_en.txt");
        let with_entry = update_mount(
            &fs::read(&mount).unwrap(),
            Some(format!("{MOUNT_PREFIX},900,AB,,,").as_str()),
        )
        .unwrap();
        fs::write(&mount, with_entry).unwrap();
        assert!(is_installed(&game));
    }

    #[test]
    fn legacy_wuwatr_install_is_recognized_and_moved() {
        let dir = tempfile::tempdir().unwrap();
        let game = fake_game(dir.path(), "1.2.3");
        fs::write(game.join(GAME_EXE), b"").unwrap();
        let resources = game.join("Client/Saved/Resources/1.2.3");
        let legacy = resources.join(LEGACY_MOUNT_DIR);
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join(LEGACY_PAK_NAME), b"eski").unwrap();
        let mount = resources.join("Mount/MountLang_en.txt");
        let old_line = format!("{LEGACY_MOUNT_PREFIX},900,AB,,,");
        let with_legacy = update_mount(&fs::read(&mount).unwrap(), Some(old_line.as_str())).unwrap();
        fs::write(&mount, with_legacy).unwrap();
        assert!(is_installed(&game));
        assert!(is_enabled(&game));

        move_legacy_pak(&resources).unwrap();
        assert!(!legacy.exists());
        assert_eq!(fs::read(resources.join(MOUNT_DIR).join(PAK_NAME)).unwrap(), b"eski");
        assert!(installed_pak(&resources).is_some());
    }

    #[test]
    fn legacy_mount_line_is_replaced_by_new_entry() {
        let entry = format!("{MOUNT_PREFIX},900,CD,,,");
        let old = format!("::Mount::\r\n{LEGACY_MOUNT_PREFIX},900,AB,,,\r\n::Del::\r\n");
        let out = update_mount(old.as_bytes(), Some(entry.as_str())).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("WuwaTR"));
        assert!(text.contains(&entry));
        assert_eq!(text.lines().filter(|l| l.starts_with("Lang_en/WuwaLang/")).count(), 1);
        let removed = update_mount(text.as_bytes(), None).unwrap();
        assert!(!mount_has_entry(&removed));
    }

    #[test]
    fn uninstall_removes_new_and_legacy_installs() {
        let dir = tempfile::tempdir().unwrap();
        let (game, resources) = installed_game(dir.path());
        let legacy = resources.join(LEGACY_MOUNT_DIR);
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join(LEGACY_PAK_NAME), b"eski").unwrap();
        let mount = resources.join("Mount/MountLang_en.txt");
        let old_line = format!("{LEGACY_MOUNT_PREFIX},900,AB,,,");
        let with_legacy = update_mount(&fs::read(&mount).unwrap(), Some(old_line.as_str())).unwrap();
        fs::write(&mount, with_legacy).unwrap();
        assert!(is_enabled(&game));

        let project = Project::new(
            game.clone(),
            dir.path().join("home"),
            dir.path().join("tools"),
        )
        .unwrap();
        project.uninstall().unwrap();
        assert!(!is_installed(&game));
        assert!(!resources.join(MOUNT_DIR).join(PAK_NAME).exists());
        assert!(!legacy.exists());
        assert_eq!(fs::read(&mount).unwrap(), MOUNT_SAMPLE.as_bytes());
    }

    #[test]
    fn legacy_pak_removal_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let resources = dir.path().join("Resources");
        let legacy = resources.join(LEGACY_MOUNT_DIR);
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join(LEGACY_PAK_NAME), b"eski").unwrap();
        remove_legacy_pak(&resources).unwrap();
        assert!(!legacy.exists());
        remove_legacy_pak(&resources).unwrap();
    }

    fn installed_game(root: &Path) -> (PathBuf, PathBuf) {
        let game = fake_game(root, "1.2.3");
        fs::write(game.join(GAME_EXE), b"").unwrap();
        let resources = game.join("Client/Saved/Resources/1.2.3");
        fs::create_dir_all(resources.join(MOUNT_DIR)).unwrap();
        fs::write(resources.join(MOUNT_DIR).join(PAK_NAME), b"pak").unwrap();
        (game, resources)
    }

    #[test]
    fn enable_and_disable_toggle_only_the_mount_line() {
        let dir = tempfile::tempdir().unwrap();
        let (game, resources) = installed_game(dir.path());
        let project = Project::new(
            game.clone(),
            dir.path().join("home"),
            dir.path().join("tools"),
        )
        .unwrap();
        let mount = resources.join("Mount/MountLang_en.txt");
        let pak = resources.join(MOUNT_DIR).join(PAK_NAME);
        assert!(!is_enabled(&game));

        project.enable().unwrap();
        assert!(is_enabled(&game) && is_installed(&game));
        let text = fs::read_to_string(&mount).unwrap();
        let sha = sha1_file(&pak).unwrap();
        assert!(text.contains(&format!("{MOUNT_PREFIX},900,{sha},,,")));
        assert_eq!(text.matches(MOUNT_PREFIX).count(), 1);

        project.enable().unwrap();
        assert_eq!(fs::read_to_string(&mount).unwrap(), text);

        project.disable().unwrap();
        assert!(!is_enabled(&game));
        assert_eq!(fs::read(&mount).unwrap(), MOUNT_SAMPLE.as_bytes());
        assert!(pak.is_file());

        project.disable().unwrap();
        assert_eq!(fs::read(&mount).unwrap(), MOUNT_SAMPLE.as_bytes());
        assert!(pak.is_file());
    }

    #[test]
    fn enable_without_package_fails_clearly() {
        let dir = tempfile::tempdir().unwrap();
        let game = fake_game(dir.path(), "1.2.3");
        fs::write(game.join(GAME_EXE), b"").unwrap();
        let project = Project::new(
            game.clone(),
            dir.path().join("home"),
            dir.path().join("tools"),
        )
        .unwrap();
        let error = project.enable().unwrap_err();
        assert!(error.to_string().contains("Türkçe paket kurulu değil"));
        assert!(!is_enabled(&game));
    }

    fn game_layout(root: &Path) -> PathBuf {
        let game = root.join("Wuthering Waves Game");
        fs::create_dir_all(game.join("Client/Saved/Resources")).unwrap();
        fs::create_dir_all(game.join("Engine")).unwrap();
        fs::write(game.join(GAME_EXE), b"").unwrap();
        game
    }

    #[test]
    fn game_root_from_launcher_exe() {
        let dir = tempfile::tempdir().unwrap();
        let game = game_layout(dir.path());
        assert_eq!(root_from_exe(&game.join(GAME_EXE)).unwrap(), game);
    }

    #[test]
    fn game_root_from_shipping_exe() {
        let dir = tempfile::tempdir().unwrap();
        let game = game_layout(dir.path());
        let shipping = game.join("Client/Binaries/Win64");
        fs::create_dir_all(&shipping).unwrap();
        fs::write(shipping.join(SHIPPING_EXE), b"").unwrap();
        assert_eq!(root_from_exe(&shipping.join(SHIPPING_EXE)).unwrap(), game);
    }

    #[test]
    fn game_root_rejects_wrong_exe_and_missing_layout() {
        let dir = tempfile::tempdir().unwrap();
        let game = game_layout(dir.path());
        fs::write(game.join("notepad.exe"), b"").unwrap();
        let wrong_name = root_from_exe(&game.join("notepad.exe")).unwrap_err();
        assert!(wrong_name.to_string().contains("notepad.exe"));

        let lone = dir.path().join("Baska Oyun");
        fs::create_dir_all(&lone).unwrap();
        fs::write(lone.join(GAME_EXE), b"").unwrap();
        let missing = root_from_exe(&lone.join(GAME_EXE)).unwrap_err();
        assert!(missing.to_string().contains("Wuthering Waves Game"));
    }

    #[test]
    fn game_dir_uses_exe_when_set_and_game_path_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let game = game_layout(dir.path());
        let mut cfg = Config {
            game_path: "C:\\Eski\\Yol".into(),
            ..Config::default()
        };
        assert_eq!(game_dir(&cfg).unwrap(), PathBuf::from("C:\\Eski\\Yol"));
        cfg.game_exe = game.join(GAME_EXE).to_string_lossy().into_owned();
        assert_eq!(game_dir(&cfg).unwrap(), game);
    }

    #[test]
    fn resource_root_picks_highest_version() {
        let dir = tempfile::tempdir().unwrap();
        let game = fake_game(dir.path(), "9.0.1");
        let newer = game.join("Client/Saved/Resources/10.0.0/Mount");
        fs::create_dir_all(&newer).unwrap();
        fs::write(newer.join("MountResource.txt"), "").unwrap();
        assert!(resource_root(&game).unwrap().ends_with("10.0.0"));
    }

    #[test]
    fn version_keys_are_numeric_only() {
        assert_eq!(version_key("1.2.3"), Some(vec![1, 2, 3]));
        assert_eq!(version_key("1"), None);
        assert_eq!(version_key("1.x"), None);
        assert_eq!(version_key("1..2"), None);
    }

    #[test]
    fn sha256_verification() {
        assert!(verify_sha256(
            b"abc",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        )
        .is_ok());
        assert!(verify_sha256(
            b"abd",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        )
        .is_err());
    }

    #[test]
    fn game_is_not_running_in_this_environment() {
        assert!(!game_running());
    }

    #[test]
    fn keys_are_validated_and_deduplicated() {
        let key = format!("0x{}", "a".repeat(64));
        let response = json!({"mainKey": key, "dynamicKeys": [{"key": key}]});
        assert_eq!(parse_keys(&response).unwrap(), vec![key.clone()]);
        let bad = json!({"mainKey": "0x12", "dynamicKeys": []});
        assert!(parse_keys(&bad).is_err());
    }
}
