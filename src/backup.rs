//! Çeviri belleği yedekleri: otomatik ve elle yedek, listeleme, budama, geri yükleme
//! ve topluluk paketi dışa aktarımı. Yedekler `backups/tm.<kod>.<YYYYMMDD-HHMMSS>.db`
//! olarak tutulur (saat UTC). Her dil için en fazla `KEEP` yedek saklanır.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use sha1::{Digest, Sha1};

use crate::tm::TranslationMemory;

/// Dil başına saklanan en fazla yedek sayısı.
pub const KEEP: usize = 10;
const DIR: &str = "backups";
const STAMP_LEN: usize = 15;

/// (dosya, tablo, kimlik, kaynak metin)
pub type Row = (String, String, String, String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub path: PathBuf,
    /// YYYYMMDD-HHMMSS (UTC)
    pub stamp: String,
    pub bytes: u64,
    /// Yedekteki kayıt sayısı; dosya okunamazsa None.
    pub records: Option<usize>,
}

pub fn dir(root: &Path) -> PathBuf {
    root.join(DIR)
}

fn marker_path(root: &Path, code: &str) -> PathBuf {
    dir(root).join(format!(".{code}.fingerprint"))
}

fn file_name(code: &str, stamp: &str) -> String {
    format!("tm.{code}.{stamp}.db")
}

/// Yedek dosya adından damgayı çıkarır. Yalnızca bu dilin yedek adlarını kabul eder.
pub fn stamp_of(name: &str, code: &str) -> Option<String> {
    let inner = name
        .strip_prefix(&format!("tm.{code}."))?
        .strip_suffix(".db")?;
    let valid = inner.len() == STAMP_LEN
        && inner.bytes().enumerate().all(|(index, byte)| {
            if index == 8 {
                byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        });
    valid.then(|| inner.to_string())
}

/// Unix saniyesini YYYYMMDD-HHMMSS (UTC) metnine çevirir.
pub fn format_stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Howard Hinnant'ın gün -> takvim algoritması (Unix epoch'tan itibaren gün sayısı).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Damgayı insan okunur biçime çevirir: "2023-11-14 22:13:20".
pub fn display_stamp(stamp: &str) -> String {
    if stamp.len() != STAMP_LEN {
        return stamp.to_string();
    }
    format!(
        "{}-{}-{} {}:{}:{}",
        &stamp[0..4],
        &stamp[4..6],
        &stamp[6..8],
        &stamp[9..11],
        &stamp[11..13],
        &stamp[13..15]
    )
}

/// Bayt sayısını okunur metne çevirir.
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let value = bytes as f64;
    if value < KB {
        format!("{bytes} B")
    } else if value < KB * KB {
        format!("{:.1} KB", value / KB)
    } else if value < KB * KB * KB {
        format!("{:.1} MB", value / (KB * KB))
    } else {
        format!("{:.1} GB", value / (KB * KB * KB))
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn count_rows(path: &Path) -> Result<usize> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let count: i64 = conn.query_row("select count(*) from tm", [], |row| row.get(0))?;
    Ok(count as usize)
}

/// TM'nin içeriğinden (kayıt sayısı, parmak izi) üretir. Değişiklik tespiti için kullanılır.
fn snapshot(tm_path: &Path) -> Result<(usize, String)> {
    let conn = Connection::open_with_flags(tm_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut hasher = Sha1::new();
    let mut count = 0;
    let mut statement = conn.prepare("select key, src, tr, engine, ts from tm order by key")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        count += 1;
        for index in 0..5 {
            let value: rusqlite::types::Value = row.get(index)?;
            hasher.update(format!("{value:?}\u{1f}"));
        }
        hasher.update(b"\n");
    }
    let digest = hasher.finalize();
    let print: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok((count, print))
}

fn info_of(path: &Path, stamp: String) -> Info {
    Info {
        path: path.to_path_buf(),
        stamp,
        bytes: fs::metadata(path).map(|meta| meta.len()).unwrap_or(0),
        records: count_rows(path).ok(),
    }
}

/// Şu anki TM'nin yedeğini alır. Değişiklik yoksa (force değilse) None döner.
pub fn create(root: &Path, code: &str, tm_path: &Path, force: bool) -> Result<Option<Info>> {
    create_at(root, code, tm_path, force, now_secs())
}

fn create_at(
    root: &Path,
    code: &str,
    tm_path: &Path,
    force: bool,
    secs: u64,
) -> Result<Option<Info>> {
    if !tm_path.is_file() {
        return Ok(None);
    }
    let (count, print) = snapshot(tm_path)?;
    let marker = marker_path(root, code);
    let unchanged = fs::read_to_string(&marker).is_ok_and(|saved| saved.trim() == print);
    if !force && (unchanged || count == 0) {
        return Ok(None);
    }
    let folder = dir(root);
    fs::create_dir_all(&folder)?;
    // Aynı saniyede yedek varsa damgayı bir saniye ileri al; böylece hiçbir yedek ezilmez.
    let mut stamp_secs = secs;
    let (stamp, target) = loop {
        let stamp = format_stamp(stamp_secs);
        let target = folder.join(file_name(code, &stamp));
        if !target.exists() {
            break (stamp, target);
        }
        stamp_secs += 1;
    };
    let temp = folder.join(format!("{}.tmp", file_name(code, &stamp)));
    let _ = fs::remove_file(&temp);
    {
        let conn = Connection::open(tm_path)?;
        conn.execute("vacuum into ?1", [temp.to_string_lossy().as_ref()])?;
    }
    fs::rename(&temp, &target)?;
    fs::write(&marker, format!("{print}\n"))?;
    prune(root, code)?;
    Ok(Some(info_of(&target, stamp)))
}

/// Otomatik yedek. Değişiklik varsa TM yedeklenir; `pack` verilirse topluluk paketi de
/// (jsonl + index.json) o klasöre yazılır.
pub fn auto(
    root: &Path,
    code: &str,
    tm_path: &Path,
    pack: Option<(&Path, &[Row])>,
) -> Result<Option<Info>> {
    let made = create(root, code, tm_path, false)?;
    if made.is_some() {
        if let Some((pack_dir, rows)) = pack {
            export_pack(tm_path, pack_dir, rows)?;
        }
    }
    Ok(made)
}

/// TM'yi topluluk paketi olarak dışa aktarır. Elle düzenlenen kayıtlar "manual": true ile yazılır.
pub fn export_pack(tm_path: &Path, pack_dir: &Path, rows: &[Row]) -> Result<usize> {
    let tm = TranslationMemory::open(tm_path)?;
    tm.export_pack(pack_dir, rows)
}

/// Dilin yedeklerini en yeniden eskiye sıralı listeler.
pub fn list(root: &Path, code: &str) -> Result<Vec<Info>> {
    let Ok(entries) = fs::read_dir(dir(root)) else {
        return Ok(Vec::new());
    };
    let mut infos = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stamp) = stamp_of(&name, code) else {
            continue;
        };
        infos.push(info_of(&entry.path(), stamp));
    }
    infos.sort_by(|a, b| b.stamp.cmp(&a.stamp));
    Ok(infos)
}

/// En yeni `KEEP` yedeği tutar, eskileri siler. Silinen yedek sayısını döndürür.
pub fn prune(root: &Path, code: &str) -> Result<usize> {
    let mut infos = list(root, code)?;
    if infos.len() <= KEEP {
        return Ok(0);
    }
    let old = infos.split_off(KEEP);
    for info in &old {
        fs::remove_file(&info.path)?;
    }
    Ok(old.len())
}

/// Seçilen yedeği TM yapar. Önce mevcut TM yedeklenir (dönen değer bu yedektir).
pub fn restore(root: &Path, code: &str, backup: &Path, tm_path: &Path) -> Result<Option<Info>> {
    count_rows(backup).context("Yedek dosyası okunamadı veya geçersiz.")?;
    let name = tm_path
        .file_name()
        .context("TM yolu geçersiz.")?
        .to_string_lossy()
        .into_owned();
    let temp = tm_path.with_file_name(format!("{name}.restoring"));
    fs::copy(backup, &temp)?;
    let pre = create(root, code, tm_path, true)?;
    fs::rename(&temp, tm_path)?;
    Ok(pre)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tm::TranslationMemory;

    fn fake(root: &Path, code: &str, stamp: &str) {
        fs::create_dir_all(dir(root)).unwrap();
        fs::write(dir(root).join(file_name(code, stamp)), b"x").unwrap();
    }

    fn tm_with(path: &Path, rows: &[(&str, &str, &str)]) -> TranslationMemory {
        let tm = TranslationMemory::open(path).unwrap();
        let rows: Vec<(String, String, String)> = rows
            .iter()
            .map(|(s, t, e)| (s.to_string(), t.to_string(), e.to_string()))
            .collect();
        tm.put_many(&rows).unwrap();
        tm
    }

    #[test]
    fn stamps_convert_to_utc_calendar() {
        assert_eq!(format_stamp(0), "19700101-000000");
        assert_eq!(format_stamp(1_700_000_000), "20231114-221320");
        assert_eq!(display_stamp("20231114-221320"), "2023-11-14 22:13:20");
        assert_eq!(display_stamp("bozuk"), "bozuk");
    }

    #[test]
    fn only_own_language_names_are_backups() {
        assert_eq!(
            stamp_of("tm.tr.20260101-120000.db", "tr").as_deref(),
            Some("20260101-120000")
        );
        assert_eq!(stamp_of("tm.tr.20260101-120000.db", "de"), None);
        assert_eq!(stamp_of("tm.pt-BR.20260101-120000.db", "pt"), None);
        assert_eq!(stamp_of("tm.tr.20260101-120000.db.tmp", "tr"), None);
        assert_eq!(stamp_of("tm.tr.bozuk.db", "tr"), None);
    }

    #[test]
    fn format_bytes_is_readable() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MB");
    }

    #[test]
    fn unchanged_tm_is_not_backed_up_twice() {
        let dir = tempfile::tempdir().unwrap();
        let tm_path = dir.path().join("tm.db");
        tm_with(&tm_path, &[("Hit", "Vuruş", "yandex")]);
        let first = create(dir.path(), "tr", &tm_path, false).unwrap();
        assert!(first.is_some());
        assert_eq!(first.unwrap().records, Some(1));
        assert!(create(dir.path(), "tr", &tm_path, false).unwrap().is_none());
        assert!(create(dir.path(), "tr", &tm_path, true).unwrap().is_some());
    }

    #[test]
    fn empty_tm_is_skipped_unless_forced() {
        let dir = tempfile::tempdir().unwrap();
        let tm_path = dir.path().join("tm.db");
        tm_with(&tm_path, &[]);
        assert!(create(dir.path(), "tr", &tm_path, false).unwrap().is_none());
        assert!(create(dir.path(), "tr", &tm_path, true).unwrap().is_some());
    }

    #[test]
    fn same_second_backups_never_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let tm_path = dir.path().join("tm.db");
        tm_with(&tm_path, &[("A", "B", "yandex")]);
        let first = create_at(dir.path(), "tr", &tm_path, true, 1_700_000_000).unwrap().unwrap();
        let second = create_at(dir.path(), "tr", &tm_path, true, 1_700_000_000).unwrap().unwrap();
        assert_eq!(first.stamp, "20231114-221320");
        assert_eq!(second.stamp, "20231114-221321");
        assert_eq!(list(dir.path(), "tr").unwrap().len(), 2);
    }

    #[test]
    fn prune_keeps_newest_ten_per_language() {
        let dir = tempfile::tempdir().unwrap();
        for minute in 0..12 {
            fake(dir.path(), "tr", &format!("20240101-0000{minute:02}"));
        }
        fake(dir.path(), "de", "20240101-000000");
        assert_eq!(prune(dir.path(), "tr").unwrap(), 2);
        let kept = list(dir.path(), "tr").unwrap();
        assert_eq!(kept.len(), KEEP);
        assert_eq!(kept[0].stamp, "20240101-000011");
        assert_eq!(kept[KEEP - 1].stamp, "20240101-000002");
        assert_eq!(list(dir.path(), "de").unwrap().len(), 1);
    }

    #[test]
    fn restore_round_trip_keeps_manual_entries_and_backs_up_current() {
        let dir = tempfile::tempdir().unwrap();
        let tm_path = dir.path().join("tm.db");
        let tm = tm_with(&tm_path, &[("Rover", "Gezgin", "yandex"), ("Lupa", "Büyüteç", "yandex")]);
        tm.set_manual("Rover", "Elle gezgin").unwrap();
        let backup = create(dir.path(), "tr", &tm_path, true).unwrap().unwrap();
        tm.set_manual("Rover", "Sonradan bozuldu").unwrap();
        tm.reset("Lupa").unwrap();
        drop(tm);

        let pre = restore(dir.path(), "tr", &backup.path, &tm_path).unwrap();
        assert!(pre.is_some(), "geri yüklemeden önce mevcut TM yedeklenmeli");
        let restored = TranslationMemory::open(&tm_path).unwrap();
        assert_eq!(restored.count().unwrap(), 2);
        let hits = restored
            .get_many(&["Rover".to_string(), "Lupa".to_string()])
            .unwrap();
        assert_eq!(hits["Rover"], "Elle gezgin");
        assert_eq!(hits["Lupa"], "Büyüteç");
        assert_eq!(list(dir.path(), "tr").unwrap().len(), 2);
    }

    #[test]
    fn restore_rejects_files_that_are_not_databases() {
        let dir = tempfile::tempdir().unwrap();
        let tm_path = dir.path().join("tm.db");
        tm_with(&tm_path, &[("A", "B", "yandex")]);
        let bogus = dir.path().join("bozuk.db");
        fs::write(&bogus, b"bu bir sqlite degil").unwrap();
        assert!(restore(dir.path(), "tr", &bogus, &tm_path).is_err());
        assert_eq!(TranslationMemory::open(&tm_path).unwrap().count().unwrap(), 1);
    }

    #[test]
    fn pack_export_marks_manual_entries() {
        let dir = tempfile::tempdir().unwrap();
        let tm_path = dir.path().join("tm.db");
        let tm = tm_with(&tm_path, &[("Rover", "Gezgin", "yandex")]);
        tm.set_manual("Rover", "Elle gezgin").unwrap();
        let rows: Vec<Row> = vec![(
            "lang.db".into(),
            "Menu".into(),
            "1".into(),
            "Rover".into(),
        )];
        let pack = dir.path().join("packs").join("tr");
        let made = auto(dir.path(), "tr", &tm_path, Some((&pack, &rows))).unwrap();
        assert!(made.is_some());
        let text = fs::read_to_string(pack.join("Menu.jsonl")).unwrap();
        assert!(text.contains("\"manual\":true"));
        assert!(pack.join("index.json").is_file());
    }
}
