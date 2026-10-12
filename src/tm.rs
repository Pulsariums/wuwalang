use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{ensure, Context, Result};
use rusqlite::{params, params_from_iter, Connection};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config;
use crate::net::{DownloadClient, Fetcher};
use crate::util::{atomic_json, sha1_bytes, sha1_text};

const CHUNK: usize = 500;
pub const INDEX_FILE: &str = "index.json";
const INDEX_VERSION: u32 = 1;
/// Elle düzenlenmiş kayıtların motor adı. Paket aktarımı bu kayıtların üzerine yazmaz.
pub const MANUAL_ENGINE: &str = "manual";

/// Düzenleyici için bir TM kaydı.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub source: String,
    pub translation: String,
    pub engine: String,
    pub ts: i64,
}

/// Düzenleyici arama filtresi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    All,
    Manual,
    Engine(String),
}

fn escape_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportStats {
    pub imported: usize,
    pub skipped: usize,
}

/// Topluluk paketi dizini: packs/tr/index.json
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackIndex {
    pub version: u32,
    pub files: Vec<PackFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackFile {
    pub name: String,
    pub sha1: String,
    pub count: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PullStats {
    pub files: usize,
    pub downloaded: usize,
    pub imported: usize,
    pub skipped: usize,
}

/// Çeviri belleği (TM): SQLite, anahtar = sha1(kaynak metin).
/// Topluluk paketleri packs/tr/<tablo>.jsonl satırlarından oluşur:
/// {"id": "<dosya>:<tablo>:<kimlik>", "src": "...", "tr": "..."}
pub struct TranslationMemory {
    conn: Connection,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

fn current_map<S: AsRef<str>>(rows: Option<&[(S, S, S, S)]>) -> Option<HashMap<String, String>> {
    rows.map(|rows| {
        rows.iter()
            .map(|(file, table, identifier, source)| {
                (
                    format!(
                        "{}:{}:{}",
                        file.as_ref(),
                        table.as_ref(),
                        identifier.as_ref()
                    ),
                    source.as_ref().to_string(),
                )
            })
            .collect()
    })
}

fn collect_lines(
    text: &str,
    current: Option<&HashMap<String, String>>,
    batch: &mut Vec<(String, String, String)>,
) -> Result<ImportStats> {
    let mut stats = ImportStats {
        imported: 0,
        skipped: 0,
    };
    for raw in text.lines() {
        if raw.trim().is_empty() {
            continue;
        }
        let item: serde_json::Value = serde_json::from_str(raw)?;
        let field = |name: &str| {
            item.get(name)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let (src, tr, ident) = (field("src"), field("tr"), field("id"));
        if src.is_empty() || tr.is_empty() {
            stats.skipped += 1;
            continue;
        }
        if let Some(current) = current {
            let stale = match current.get(&ident) {
                Some(now) => sha1_text(now) != sha1_text(&src),
                None => true,
            };
            if stale {
                stats.skipped += 1;
                continue;
            }
        }
        batch.push((src, tr, "pack".to_string()));
        stats.imported += 1;
    }
    Ok(stats)
}

fn is_pack_name(name: &str) -> bool {
    name.ends_with(".jsonl") && !name.contains(['/', '\\'])
}

pub fn default_cache_dir() -> PathBuf {
    config::project_dir().join("packs").join(".cache")
}

impl TranslationMemory {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "create table if not exists tm (\
             key text primary key, src text not null, tr text not null,\
             engine text not null, ts integer not null)",
        )?;
        Ok(Self { conn })
    }

    /// Kaynak metin -> Türkçe metin eşlemesi (yalnızca isabetler).
    /// Elle düzenlenmiş kayıtlar da isabet sayılır; böylece çeviri onları atlar.
    pub fn get_many<S: AsRef<str>>(&self, sources: &[S]) -> Result<HashMap<String, String>> {
        Ok(self
            .lookup(sources)?
            .into_iter()
            .map(|(source, (tr, _))| (source, tr))
            .collect())
    }

    /// Kaynak metin -> (Türkçe metin, motor adı) eşlemesi.
    fn lookup<S: AsRef<str>>(&self, sources: &[S]) -> Result<HashMap<String, (String, String)>> {
        let mut keys: HashMap<String, String> = HashMap::new();
        for source in sources {
            keys.entry(sha1_text(source.as_ref()))
                .or_insert_with(|| source.as_ref().to_string());
        }
        let key_list: Vec<String> = keys.keys().cloned().collect();
        let mut result = HashMap::new();
        for chunk in key_list.chunks(CHUNK) {
            let marks = vec!["?"; chunk.len()].join(",");
            let sql = format!("select key, tr, engine from tm where key in ({marks})");
            let mut statement = self.conn.prepare(&sql)?;
            let rows = statement.query_map(
                params_from_iter(chunk.iter().map(String::as_str)),
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )?;
            for row in rows {
                let (key, tr, engine) = row?;
                if let Some(source) = keys.get(&key) {
                    result.insert(source.clone(), (tr, engine));
                }
            }
        }
        Ok(result)
    }

    /// Kayıtları metinde arar (kaynak veya çeviri, büyük/küçük harf ASCII'de duyarsız).
    /// Sonuçlar kaynağa göre sıralanır; sayfalama limit/offset ile yapılır.
    pub fn search(
        &self,
        query: &str,
        filter: &Filter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Entry>> {
        let pattern = format!("%{}%", escape_like(query.trim()));
        let engine = match filter {
            Filter::All => None,
            Filter::Manual => Some(MANUAL_ENGINE.to_string()),
            Filter::Engine(name) => Some(name.clone()),
        };
        let mut statement = self.conn.prepare(
            "select src, tr, engine, ts from tm \
             where (src like ?1 escape '\\' or tr like ?1 escape '\\') \
             and (?2 is null or engine = ?2) \
             order by src collate nocase limit ?3 offset ?4",
        )?;
        let rows = statement.query_map(
            params![pattern, engine, limit as i64, offset as i64],
            |row| {
                Ok(Entry {
                    source: row.get(0)?,
                    translation: row.get(1)?,
                    engine: row.get(2)?,
                    ts: row.get(3)?,
                })
            },
        )?;
        rows.collect::<rusqlite::Result<Vec<Entry>>>()
            .map_err(Into::into)
    }

    /// Kaydı elle düzeltilmiş olarak yazar (motor adı "manual"). Var olan kaydın yerini alır.
    pub fn set_manual(&self, source: &str, translation: &str) -> Result<()> {
        ensure!(!source.is_empty(), "Kaynak metin boş olamaz.");
        ensure!(!translation.trim().is_empty(), "Çeviri boş olamaz.");
        self.conn.execute(
            "insert or replace into tm (key, src, tr, engine, ts) values (?1,?2,?3,?4,?5)",
            params![
                sha1_text(source),
                source,
                translation,
                MANUAL_ENGINE,
                now_secs()
            ],
        )?;
        Ok(())
    }

    /// Kaydı siler; bir sonraki çeviride metin yeniden çevrilir. Silinen kayıt varsa true döner.
    pub fn reset(&self, source: &str) -> Result<bool> {
        let removed = self.conn.execute(
            "delete from tm where key = ?1",
            params![sha1_text(source)],
        )?;
        Ok(removed > 0)
    }

    /// (kaynak, hedef, motor) satırlarını yazar. Var olan kayıt değişmez.
    pub fn put_many<S: AsRef<str>>(&self, rows: &[(S, S, S)]) -> Result<usize> {
        let now = now_secs();
        let payload: Vec<(String, &str, &str, &str, i64)> = rows
            .iter()
            .filter(|(_, tr, _)| !tr.as_ref().is_empty())
            .map(|(src, tr, engine)| {
                (
                    sha1_text(src.as_ref()),
                    src.as_ref(),
                    tr.as_ref(),
                    engine.as_ref(),
                    now,
                )
            })
            .collect();
        if payload.is_empty() {
            return Ok(0);
        }
        let before = self.conn.total_changes();
        let transaction = self.conn.unchecked_transaction()?;
        {
            let mut statement = transaction.prepare(
                "insert or ignore into tm (key, src, tr, engine, ts) values (?1,?2,?3,?4,?5)",
            )?;
            for (key, src, tr, engine, ts) in &payload {
                statement.execute(params![key, src, tr, engine, ts])?;
            }
        }
        transaction.commit()?;
        Ok((self.conn.total_changes() - before) as usize)
    }

    pub fn count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("select count(*) from tm", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Mevcut oyun satırlarından TM isabetlerini tablo bazlı JSONL olarak yazar ve index.json ekler.
    /// rows: (dosya, tablo, kimlik, kaynak metin)
    pub fn export_pack<S: AsRef<str>>(
        &self,
        directory: &Path,
        rows: &[(S, S, S, S)],
    ) -> Result<usize> {
        fs::create_dir_all(directory)?;
        let sources: Vec<&str> = rows
            .iter()
            .map(|(_, _, _, source)| source.as_ref())
            .collect();
        let hits = self.lookup(&sources)?;
        let mut by_table: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (file, table, identifier, source) in rows {
            let Some((tr, engine)) = hits.get(source.as_ref()) else {
                continue;
            };
            let mut line = json!({
                "id": format!("{}:{}:{}", file.as_ref(), table.as_ref(), identifier.as_ref()),
                "src": source.as_ref(),
                "tr": tr,
            });
            if engine == MANUAL_ENGINE {
                line["manual"] = json!(true);
            }
            let line = line.to_string();
            by_table
                .entry(table.as_ref().to_string())
                .or_default()
                .push(line);
        }
        let mut written = 0;
        let mut files = Vec::with_capacity(by_table.len());
        for (table, lines) in &by_table {
            let name = format!("{table}.jsonl");
            let content = format!("{}\n", lines.join("\n"));
            let target = directory.join(&name);
            let tmp = directory.join(format!("{name}.tmp"));
            fs::write(&tmp, &content)?;
            fs::rename(&tmp, &target)?;
            files.push(PackFile {
                name,
                sha1: sha1_text(&content),
                count: lines.len(),
            });
            written += lines.len();
        }
        atomic_json(
            &directory.join(INDEX_FILE),
            &PackIndex {
                version: INDEX_VERSION,
                files,
            },
        )?;
        Ok(written)
    }

    /// Paketi TM'e alır. rows verilirse her satırın kimliği için oyundaki güncel kaynak metnin
    /// sha1'i paketteki kaynakla aynı olmalıdır; değilse satır atlanır.
    pub fn import_pack<S: AsRef<str>>(
        &self,
        directory: &Path,
        rows: Option<&[(S, S, S, S)]>,
    ) -> Result<ImportStats> {
        let current = current_map(rows);
        let mut files: Vec<PathBuf> = match fs::read_dir(directory) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
                .collect(),
            Err(_) => Vec::new(),
        };
        files.sort();

        let mut total = ImportStats {
            imported: 0,
            skipped: 0,
        };
        let mut batch: Vec<(String, String, String)> = Vec::new();
        for path in &files {
            let text = fs::read_to_string(path)?;
            let stats = collect_lines(&text, current.as_ref(), &mut batch)?;
            total.imported += stats.imported;
            total.skipped += stats.skipped;
        }
        self.put_many(&batch)?;
        Ok(total)
    }
}

/// Uzak paketi (index.json + değişen *.jsonl dosyaları) TM'e çeker.
/// Dosya sha1'i önbellekteki (`packs/.cache/<ad>.sha1`) değerle aynıysa dosya indirilmez.
pub fn pull_pack<S: AsRef<str>>(
    url: &str,
    tm: &TranslationMemory,
    rows: Option<&[(S, S, S, S)]>,
    log: &dyn Fn(&str),
) -> Result<PullStats> {
    pull_pack_in(url, tm, &default_cache_dir(), rows, log)
}

pub fn pull_pack_in<S: AsRef<str>>(
    url: &str,
    tm: &TranslationMemory,
    cache: &Path,
    rows: Option<&[(S, S, S, S)]>,
    log: &dyn Fn(&str),
) -> Result<PullStats> {
    let client = DownloadClient::new();
    let sleep = |wait: Duration| thread::sleep(wait);
    let fetcher = Fetcher {
        client: &client,
        log,
        sleep: &sleep,
    };
    pull_pack_with(&fetcher, url, tm, cache, rows)
}

pub fn pull_pack_with<S: AsRef<str>>(
    fetcher: &Fetcher,
    url: &str,
    tm: &TranslationMemory,
    cache: &Path,
    rows: Option<&[(S, S, S, S)]>,
) -> Result<PullStats> {
    let base = url.trim_end_matches('/');
    let index: PackIndex = serde_json::from_slice(&fetcher.get(&format!("{base}/{INDEX_FILE}"))?)
        .context("index.json çözümlenemedi.")?;
    ensure!(
        index.version == INDEX_VERSION,
        "Desteklenmeyen paket sürümü: {}",
        index.version
    );
    let current = current_map(rows);
    fs::create_dir_all(cache)?;
    let mut stats = PullStats::default();
    for file in &index.files {
        ensure!(
            is_pack_name(&file.name),
            "Geçersiz paket dosya adı: {}",
            file.name
        );
        stats.files += 1;
        let marker = cache.join(format!("{}.sha1", file.name));
        let cached = fs::read_to_string(&marker).unwrap_or_default();
        if cached.trim() == file.sha1 {
            (fetcher.log)(&format!("{}: değişmedi, atlandı.", file.name));
            continue;
        }
        let bytes = fetcher.get(&format!("{base}/{}", file.name))?;
        ensure!(
            sha1_bytes(&bytes) == file.sha1,
            "{} sha1 uyuşmuyor; uzak paket bozuk olabilir.",
            file.name
        );
        let text =
            String::from_utf8(bytes).with_context(|| format!("{} UTF-8 değil.", file.name))?;
        let mut batch = Vec::new();
        let part = collect_lines(&text, current.as_ref(), &mut batch)?;
        tm.put_many(&batch)?;
        stats.imported += part.imported;
        stats.skipped += part.skipped;
        let tmp = cache.join(format!("{}.sha1.tmp", file.name));
        fs::write(&tmp, &file.sha1)?;
        fs::rename(&tmp, &marker)?;
        stats.downloaded += 1;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn put_get_dedupes_across_sources() {
        let dir = tempfile::tempdir().unwrap();
        let tm = TranslationMemory::open(&dir.path().join("tm.db")).unwrap();
        assert_eq!(
            tm.put_many(&[("Hello", "Merhaba", "anthropic")]).unwrap(),
            1
        );
        assert_eq!(tm.put_many(&[("Hello", "Selam", "other")]).unwrap(), 0);
        let hits = tm.get_many(&["Hello", "Bye"]).unwrap();
        assert_eq!(
            hits,
            HashMap::from([("Hello".to_string(), "Merhaba".to_string())])
        );
        assert_eq!(tm.count().unwrap(), 1);
    }

    #[test]
    fn export_then_import_checks_source_hash() {
        let dir = tempfile::tempdir().unwrap();
        let tm = TranslationMemory::open(&dir.path().join("tm.db")).unwrap();
        tm.put_many(&[("Hello", "Merhaba", "x")]).unwrap();
        let rows = [("f.db", "Text", "1", "Hello"), ("f.db", "Text", "2", "Bye")];
        let pack = dir.path().join("pack");
        assert_eq!(tm.export_pack(&pack, &rows).unwrap(), 1);
        let content = fs::read_to_string(pack.join("Text.jsonl")).unwrap();
        let line: serde_json::Value =
            serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert_eq!(
            line,
            json!({"id": "f.db:Text:1", "src": "Hello", "tr": "Merhaba"})
        );

        let fresh = TranslationMemory::open(&dir.path().join("fresh.db")).unwrap();
        assert_eq!(
            fresh
                .import_pack(&pack, None::<&[(&str, &str, &str, &str)]>)
                .unwrap(),
            ImportStats {
                imported: 1,
                skipped: 0
            }
        );
        assert_eq!(
            fresh.get_many(&["Hello"]).unwrap(),
            HashMap::from([("Hello".to_string(), "Merhaba".to_string())])
        );

        let changed_rows = [("f.db", "Text", "1", "Hello there")];
        let other = TranslationMemory::open(&dir.path().join("other.db")).unwrap();
        assert_eq!(
            other.import_pack(&pack, Some(&changed_rows)).unwrap(),
            ImportStats {
                imported: 0,
                skipped: 1
            }
        );
        assert_eq!(other.count().unwrap(), 0);
    }

    #[test]
    fn export_writes_index_matching_files() {
        let dir = tempfile::tempdir().unwrap();
        let tm = TranslationMemory::open(&dir.path().join("tm.db")).unwrap();
        tm.put_many(&[("Hello", "Merhaba", "x"), ("Bye", "Hoşça kal", "x")])
            .unwrap();
        let rows = [
            ("f.db", "Text", "1", "Hello"),
            ("f.db", "Text", "2", "Bye"),
            ("f.db", "Menu", "3", "Missing"),
        ];
        let pack = dir.path().join("pack");
        assert_eq!(tm.export_pack(&pack, &rows).unwrap(), 2);

        let index: PackIndex =
            serde_json::from_slice(&fs::read(pack.join(INDEX_FILE)).unwrap()).unwrap();
        assert_eq!(index.version, 1);
        let content = fs::read(pack.join("Text.jsonl")).unwrap();
        assert_eq!(
            index.files,
            vec![PackFile {
                name: "Text.jsonl".into(),
                sha1: sha1_bytes(&content),
                count: 2,
            }]
        );
    }

    struct Host {
        files: HashMap<String, Vec<u8>>,
        calls: RefCell<Vec<String>>,
    }

    impl crate::net::HttpClient for Host {
        fn get(&self, url: &str) -> Result<Vec<u8>> {
            self.calls.borrow_mut().push(url.to_owned());
            self.files
                .get(url)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("404 {url}"))
        }
    }

    fn pack_file(name: &str, content: &[u8]) -> PackFile {
        PackFile {
            name: name.into(),
            sha1: sha1_bytes(content),
            count: 1,
        }
    }

    fn host_for(base: &str, text_line: &str, menu_line: &str) -> Host {
        let text_bytes = format!("{text_line}\n").into_bytes();
        let menu_bytes = format!("{menu_line}\n").into_bytes();
        let index = PackIndex {
            version: 1,
            files: vec![
                pack_file("Text.jsonl", &text_bytes),
                pack_file("Menu.jsonl", &menu_bytes),
            ],
        };
        Host {
            files: HashMap::from([
                (
                    format!("{base}/index.json"),
                    serde_json::to_vec(&index).unwrap(),
                ),
                (format!("{base}/Text.jsonl"), text_bytes),
                (format!("{base}/Menu.jsonl"), menu_bytes),
            ]),
            calls: RefCell::new(Vec::new()),
        }
    }

    #[test]
    fn pull_downloads_only_changed_files() {
        let dir = tempfile::tempdir().unwrap();
        let base = "https://packs.test/tr";
        let rows = [
            ("f.db", "Text", "1", "Hello"),
            ("f.db", "Menu", "2", "Oyun"),
        ];
        let tm = TranslationMemory::open(&dir.path().join("tm.db")).unwrap();
        let cache = dir.path().join("cache");
        let log = |_: &str| {};
        let sleep = |_: Duration| {};

        let text_v1 = r#"{"id":"f.db:Text:1","src":"Hello","tr":"Merhaba"}"#;
        let menu_line = r#"{"id":"f.db:Menu:2","src":"Oyun","tr":"Oyun"}"#;
        let host = host_for(base, text_v1, menu_line);
        let fetcher = Fetcher {
            client: &host,
            log: &log,
            sleep: &sleep,
        };
        let first = pull_pack_with(&fetcher, base, &tm, &cache, Some(&rows[..])).unwrap();
        assert_eq!(
            first,
            PullStats {
                files: 2,
                downloaded: 2,
                imported: 2,
                skipped: 0
            }
        );
        assert_eq!(tm.count().unwrap(), 2);
        assert_eq!(host.calls.borrow().len(), 3);

        let text_v2 = r#"{"id":"f.db:Text:1","src":"Hello","tr":"Selam"}"#;
        let host = host_for(base, text_v2, menu_line);
        let fetcher = Fetcher {
            client: &host,
            log: &log,
            sleep: &sleep,
        };
        let second = pull_pack_with(&fetcher, base, &tm, &cache, Some(&rows[..])).unwrap();
        assert_eq!(second.files, 2);
        assert_eq!(second.downloaded, 1);
        let calls = host.calls.borrow();
        assert!(calls.iter().any(|url| url.ends_with("Text.jsonl")));
        assert!(!calls.iter().any(|url| url.ends_with("Menu.jsonl")));
    }

    #[test]
    fn pull_rejects_hash_mismatch_and_skips_stale_rows() {
        let dir = tempfile::tempdir().unwrap();
        let base = "https://packs.test/tr";
        let tm = TranslationMemory::open(&dir.path().join("tm.db")).unwrap();
        let log = |_: &str| {};
        let sleep = |_: Duration| {};

        let mut host = host_for(
            base,
            r#"{"id":"f.db:Text:1","src":"Hello","tr":"Merhaba"}"#,
            "",
        );
        host.files
            .insert(format!("{base}/Menu.jsonl"), b"tampered\n".to_vec());
        let fetcher = Fetcher {
            client: &host,
            log: &log,
            sleep: &sleep,
        };
        let error = pull_pack_with(
            &fetcher,
            base,
            &tm,
            &dir.path().join("cache-a"),
            None::<&[(&str, &str, &str, &str)]>,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("Menu.jsonl sha1 uyuşmuyor"), "{error}");

        let fresh = TranslationMemory::open(&dir.path().join("fresh.db")).unwrap();
        let rows = [("f.db", "Text", "1", "Hello there")];
        let host = host_for(
            base,
            r#"{"id":"f.db:Text:1","src":"Hello","tr":"Merhaba"}"#,
            "",
        );
        let fetcher = Fetcher {
            client: &host,
            log: &log,
            sleep: &sleep,
        };
        let stats = pull_pack_with(
            &fetcher,
            base,
            &fresh,
            &dir.path().join("cache-b"),
            Some(&rows[..]),
        )
        .unwrap();
        assert_eq!(stats.downloaded, 2);
        assert_eq!(stats.imported, 0);
        assert_eq!(stats.skipped, 1);
        assert_eq!(fresh.count().unwrap(), 0);
    }

    #[test]
    fn manual_entries_survive_import_and_pull() {
        let dir = tempfile::tempdir().unwrap();
        let tm = TranslationMemory::open(&dir.path().join("tm.db")).unwrap();
        tm.set_manual("Hello", "Elle yazıldı").unwrap();
        let pack = dir.path().join("pack");
        fs::create_dir_all(&pack).unwrap();
        fs::write(
            pack.join("Text.jsonl"),
            "{\"id\":\"f.db:Text:1\",\"src\":\"Hello\",\"tr\":\"Merhaba\"}\n",
        )
        .unwrap();
        tm.import_pack(&pack, None::<&[(&str, &str, &str, &str)]>)
            .unwrap();
        assert_eq!(tm.get_many(&["Hello"]).unwrap()["Hello"], "Elle yazıldı");

        let base = "https://packs.test/tr";
        let rows = [("f.db", "Text", "1", "Hello")];
        let host = host_for(
            base,
            r#"{"id":"f.db:Text:1","src":"Hello","tr":"Merhaba"}"#,
            "",
        );
        let log = |_: &str| {};
        let sleep = |_: Duration| {};
        let fetcher = Fetcher {
            client: &host,
            log: &log,
            sleep: &sleep,
        };
        pull_pack_with(
            &fetcher,
            base,
            &tm,
            &dir.path().join("cache"),
            Some(&rows[..]),
        )
        .unwrap();
        assert_eq!(tm.get_many(&["Hello"]).unwrap()["Hello"], "Elle yazıldı");
        assert_eq!(tm.search("", &Filter::Manual, 10, 0).unwrap().len(), 1);
    }

    #[test]
    fn export_marks_manual_entries_only() {
        let dir = tempfile::tempdir().unwrap();
        let tm = TranslationMemory::open(&dir.path().join("tm.db")).unwrap();
        tm.put_many(&[("Bye", "Hoşça kal", "anthropic")]).unwrap();
        tm.set_manual("Hello", "Selam").unwrap();
        let rows = [("f.db", "Text", "1", "Hello"), ("f.db", "Text", "2", "Bye")];
        let pack = dir.path().join("pack");
        assert_eq!(tm.export_pack(&pack, &rows).unwrap(), 2);
        let content = fs::read_to_string(pack.join("Text.jsonl")).unwrap();
        let lines: Vec<serde_json::Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines[0]["manual"], json!(true));
        assert_eq!(lines[1].get("manual"), None);
        assert_eq!(
            lines[1],
            json!({"id": "f.db:Text:2", "src": "Bye", "tr": "Hoşça kal"})
        );
    }

    #[test]
    fn search_filters_pages_and_escapes_wildcards() {
        let dir = tempfile::tempdir().unwrap();
        let tm = TranslationMemory::open(&dir.path().join("tm.db")).unwrap();
        tm.put_many(&[
            ("Rover waits", "Gezgin bekler", "anthropic"),
            ("100% done", "Yüzde 100", "google"),
            ("Other", "Diğer", "anthropic"),
        ])
        .unwrap();
        tm.set_manual("Rover falls", "Gezgin düşer").unwrap();

        let all = tm.search("rover", &Filter::All, 10, 0).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].source, "Rover falls");
        let by_tr = tm.search("bekler", &Filter::All, 10, 0).unwrap();
        assert_eq!(by_tr.len(), 1);
        assert_eq!(by_tr[0].engine, "anthropic");
        let google = tm.search("", &Filter::Engine("google".into()), 10, 0).unwrap();
        assert_eq!(google.len(), 1);
        assert_eq!(tm.search("", &Filter::All, 2, 0).unwrap().len(), 2);
        assert_eq!(tm.search("", &Filter::All, 2, 2).unwrap().len(), 2);
        assert_eq!(tm.search("%", &Filter::All, 10, 0).unwrap().len(), 1);
        assert_eq!(tm.search("_", &Filter::All, 10, 0).unwrap().len(), 0);
    }

    #[test]
    fn reset_removes_entry_so_it_is_retranslated() {
        let dir = tempfile::tempdir().unwrap();
        let tm = TranslationMemory::open(&dir.path().join("tm.db")).unwrap();
        tm.set_manual("Hello", "Selam").unwrap();
        assert!(tm.reset("Hello").unwrap());
        assert!(!tm.reset("Hello").unwrap());
        assert!(tm.get_many(&["Hello"]).unwrap().is_empty());
        assert!(tm.set_manual("Hello", "  ").is_err());
    }
}
