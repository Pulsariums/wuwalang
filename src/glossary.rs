use std::cell::OnceCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use serde::de::{Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};

use crate::backup::format_stamp;
use crate::config::project_dir;
use crate::util::atomic_json;

const MAX_TERM_WORDS: usize = 4;
const MAX_TERM_CHARS: usize = 40;
const MAX_TR_CHARS: usize = 60;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entry {
    pub tr: String,
    pub locked: bool,
    /// Terim modelin önerisiyle eklendi (merge_new). Elle eklenenler false.
    pub auto: bool,
}

#[derive(Deserialize)]
struct RawEntry {
    tr: String,
    #[serde(default)]
    locked: bool,
    /// Eski dosyalarda `from_model` adıyla yazılmıştı.
    #[serde(default, alias = "from_model")]
    auto: Option<bool>,
}

impl<'de> Deserialize<'de> for Entry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = RawEntry::deserialize(deserializer)?;
        Ok(Entry {
            // Bayrak yoksa (eski dosya) kilitsiz girdiler otomatik sayılır.
            auto: raw.auto.unwrap_or(!raw.locked),
            tr: raw.tr,
            locked: raw.locked,
        })
    }
}

/// Terim ve karşılığın sözlüğe girebilir olup olmadığı: kısa bir isim ve kısa bir karşılık,
/// cümle işareti yok. Cümleler, uzun açıklamalar ve satır sonları reddedilir.
pub fn is_valid_term(term: &str, tr: &str) -> bool {
    let term = term.trim();
    let tr = tr.trim();
    let has_sentence_mark = |text: &str| {
        text.chars()
            .any(|c| matches!(c, '.' | '!' | '?' | '…' | ';' | ':' | '\n' | '\r'))
    };
    !term.is_empty()
        && !tr.is_empty()
        && term.split_whitespace().count() <= MAX_TERM_WORDS
        && term.chars().count() <= MAX_TERM_CHARS
        && tr.chars().count() <= MAX_TR_CHARS
        && !has_sentence_mark(term)
        && !has_sentence_mark(tr)
}

/// merge_new sonucu: eklenen terimler ve doğrulamadan geçemeyen öneri sayısı.
#[derive(Debug, Default, PartialEq)]
pub struct Merge {
    pub added: Vec<String>,
    pub rejected: usize,
}

/// clean sonucu. `removed` silinen (veya silinecek) terimler; `kept` kalan kayıt sayısı;
/// `locked_kept` geçersiz olsa da kilitli olduğu için korunan kayıt sayısı.
#[derive(Debug, Default, PartialEq)]
pub struct CleanReport {
    pub removed: Vec<String>,
    pub kept: usize,
    pub locked_kept: usize,
}

pub fn default_path() -> PathBuf {
    project_dir().join("glossary.json")
}

pub fn fold(c: char) -> char {
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(single), None) => single,
        _ => c,
    }
}

pub fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Büyük/küçük harf duyarsız, kelime sınırlı, en uzun eşleşmeyi tercih eden terim eşleştirici.
/// Python'daki `(?<!\w)(?:...)(?!\w)` desenlerinin karakter tabanlı karşılığıdır.
pub(crate) struct TermMatcher {
    items: Vec<(Vec<char>, String)>,
    by_first: HashMap<char, Vec<usize>>,
}

impl TermMatcher {
    pub(crate) fn new<'a, I: IntoIterator<Item = &'a str>>(terms: I) -> Self {
        let mut items: Vec<(Vec<char>, String)> = terms
            .into_iter()
            .filter(|term| !term.is_empty())
            .map(|term| (term.chars().map(fold).collect(), term.to_string()))
            .collect();
        items.sort_by_key(|(folded, _)| std::cmp::Reverse(folded.len()));
        let mut by_first: HashMap<char, Vec<usize>> = HashMap::new();
        for (idx, (folded, _)) in items.iter().enumerate() {
            by_first.entry(folded[0]).or_default().push(idx);
        }
        Self { items, by_first }
    }

    /// `start` konumunda biten en uzun terimi döndürür: (bitiş, terim indeksi).
    pub(crate) fn match_at(&self, chars: &[char], start: usize) -> Option<(usize, usize)> {
        if start >= chars.len() || (start > 0 && is_word(chars[start - 1])) {
            return None;
        }
        let candidates = self.by_first.get(&fold(chars[start]))?;
        for &idx in candidates {
            let folded = &self.items[idx].0;
            let end = start + folded.len();
            if end > chars.len() {
                continue;
            }
            let same = folded
                .iter()
                .zip(&chars[start..end])
                .all(|(term, c)| *term == fold(*c));
            if !same || (end < chars.len() && is_word(chars[end])) {
                continue;
            }
            return Some((end, idx));
        }
        None
    }

    /// Soldan sağa, çakışmasız eşleşmeler: (başlangıç, bitiş, terim indeksi).
    pub(crate) fn find_all(&self, chars: &[char]) -> Vec<(usize, usize, usize)> {
        let mut found = Vec::new();
        let mut pos = 0;
        while pos < chars.len() {
            match self.match_at(chars, pos) {
                Some((end, idx)) => {
                    found.push((pos, end, idx));
                    pos = end;
                }
                None => pos += 1,
            }
        }
        found
    }
}

pub struct Glossary {
    path: PathBuf,
    conflicts_path: PathBuf,
    order: Vec<String>,
    entries: HashMap<String, Entry>,
    lower: HashMap<String, String>,
    index: OnceCell<TermMatcher>,
}

impl Glossary {
    pub fn open(path: &Path) -> Result<Self> {
        let mut glossary = Self {
            path: path.to_path_buf(),
            conflicts_path: path.with_file_name("conflicts.log"),
            order: Vec::new(),
            entries: HashMap::new(),
            lower: HashMap::new(),
            index: OnceCell::new(),
        };
        if path.exists() {
            let text = fs::read_to_string(path)?;
            let raw: Ordered = serde_json::from_str(&text)?;
            for (term, entry) in raw.0 {
                glossary.set(term, entry);
            }
        }
        Ok(glossary)
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    fn find_key(&self, term: &str) -> Option<String> {
        if self.entries.contains_key(term) {
            return Some(term.to_string());
        }
        self.lower.get(&term.to_lowercase()).cloned()
    }

    /// Tüm girdiler (terim, karşılık ve kilit durumu), eklenme sırasıyla.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Entry)> {
        self.order
            .iter()
            .map(|term| (term.as_str(), &self.entries[term]))
    }

    pub fn get(&self, term: &str) -> Option<&str> {
        let key = self.find_key(term)?;
        self.entries.get(&key).map(|entry| entry.tr.as_str())
    }

    /// Metinlerde geçen terimlerin EN -> TR eşlemesi, terim adına göre sıralı.
    pub fn subset_for<I, S>(&self, texts: I) -> Vec<(String, String)>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let matcher = self
            .index
            .get_or_init(|| TermMatcher::new(self.order.iter().map(String::as_str)));
        let mut found: BTreeMap<String, String> = BTreeMap::new();
        for text in texts {
            let chars: Vec<char> = text.as_ref().chars().collect();
            for (start, end, _) in matcher.find_all(&chars) {
                let matched: String = chars[start..end].iter().collect();
                if let Some(key) = self.lower.get(&matched.to_lowercase()) {
                    if let Some(entry) = self.entries.get(key) {
                        found.insert(key.clone(), entry.tr.clone());
                    }
                }
            }
        }
        let mut pairs: Vec<(String, String)> = found.into_iter().collect();
        pairs.sort_by_key(|(term, _)| term.to_lowercase());
        pairs
    }

    /// Tüm sözlüğün "EN=TR" satırları, terim adına göre sıralı.
    pub fn lines(&self) -> String {
        let mut terms: Vec<&String> = self.order.iter().collect();
        terms.sort_by_key(|term| term.to_lowercase());
        terms
            .into_iter()
            .map(|term| format!("{}={}", term, self.entries[term].tr))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn set(&mut self, term: String, entry: Entry) {
        if let Some(existing) = self.entries.get_mut(&term) {
            *existing = entry;
            return;
        }
        self.lower.insert(term.to_lowercase(), term.clone());
        self.order.push(term.clone());
        self.entries.insert(term, entry);
        self.index = OnceCell::new();
    }

    /// Kullanıcının açık kararı: mevcut karşılığın üzerine yazar.
    pub fn add(&mut self, term: &str, tr: &str, locked: bool) -> Result<()> {
        if term.trim().is_empty() || tr.trim().is_empty() {
            bail!("Terim ve karşılık boş olamaz.");
        }
        if !is_valid_term(term, tr) {
            bail!("Geçersiz terim: en fazla 4 kelime ve cümle işareti içermeyen bir isim girin.");
        }
        self.set(
            term.trim().to_string(),
            Entry {
                tr: tr.trim().to_string(),
                locked,
                auto: false,
            },
        );
        self.save()
    }

    pub fn lock(&mut self, term: &str) -> Result<()> {
        self.set_locked(term, true)
    }

    pub fn set_locked(&mut self, term: &str, locked: bool) -> Result<()> {
        let Some(key) = self.find_key(term) else {
            bail!("Sözlükte yok: {term}");
        };
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.locked = locked;
        }
        self.save()
    }

    /// Terimi siler. Sözlükte yoksa hata döner.
    pub fn remove(&mut self, term: &str) -> Result<()> {
        let Some(key) = self.find_key(term) else {
            bail!("Sözlükte yok: {term}");
        };
        self.entries.remove(&key);
        self.lower.remove(&key.to_lowercase());
        self.order.retain(|existing| *existing != key);
        self.index = OnceCell::new();
        self.save()
    }

    /// Modelin önerdiği yeni terimleri ekler. Mevcut terim asla değişmez (ilk karar kazanır);
    /// farklı karşılık önerilirse conflicts.log dosyasına yazılır. Geçersiz öneriler
    /// (cümle, uzun metin, noktalama) eklenmez ve `rejected` sayısına yazılır.
    pub fn merge_new(&mut self, new_terms: &BTreeMap<String, String>) -> Result<Merge> {
        let mut added = Vec::new();
        let mut rejected = 0;
        let mut conflicts = Vec::new();
        for (raw_term, raw_tr) in new_terms {
            let term = raw_term.trim();
            let tr = raw_tr.trim();
            if !is_valid_term(term, tr) {
                rejected += 1;
                continue;
            }
            if let Some(found) = self.find_key(term) {
                let current = self.entries[&found].tr.clone();
                if current != tr {
                    conflicts.push(format!(
                        "{found}: mevcut={} önerilen={}",
                        py_repr(&current),
                        py_repr(tr)
                    ));
                }
                continue;
            }
            self.set(
                term.to_string(),
                Entry {
                    tr: tr.to_string(),
                    locked: false,
                    auto: true,
                },
            );
            added.push(term.to_string());
        }
        if !conflicts.is_empty() {
            let mut log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.conflicts_path)?;
            writeln!(log, "{}", conflicts.join("\n"))?;
        }
        if !added.is_empty() {
            self.save()?;
        }
        Ok(Merge { added, rejected })
    }

    /// Geçersiz ve kilitsiz girdileri siler; kilitli girdiler korunur. Gerçek temizlikte
    /// önce `glossary.json.bak-<zaman>` yedeği yazılır. `dry_run` dosyaya dokunmaz.
    pub fn clean(&mut self, dry_run: bool) -> Result<CleanReport> {
        let mut removed = Vec::new();
        let mut locked_kept = 0;
        for (term, entry) in self.iter() {
            if is_valid_term(term, &entry.tr) {
                continue;
            }
            if entry.locked {
                locked_kept += 1;
            } else {
                removed.push(term.to_string());
            }
        }
        let kept = self.len() - removed.len();
        if !dry_run && !removed.is_empty() {
            let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            let mut backup = self.path.as_os_str().to_owned();
            backup.push(format!(".bak-{}", format_stamp(secs)));
            if self.path.exists() {
                fs::copy(&self.path, PathBuf::from(backup))?;
            }
            let gone: HashSet<&String> = removed.iter().collect();
            self.order.retain(|term| !gone.contains(term));
            for term in &removed {
                self.entries.remove(term);
                self.lower.remove(&term.to_lowercase());
            }
            self.index = OnceCell::new();
            self.save()?;
        }
        Ok(CleanReport {
            removed,
            kept,
            locked_kept,
        })
    }

    pub fn save(&self) -> Result<()> {
        atomic_json(&self.path, self)
    }
}

impl Serialize for Glossary {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.order.len()))?;
        for term in &self.order {
            map.serialize_entry(term, &self.entries[term])?;
        }
        map.end()
    }
}

struct Ordered(Vec<(String, Entry)>);

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct OrderedVisitor;

        impl<'de> Visitor<'de> for OrderedVisitor {
            type Value = Ordered;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("terim -> {tr, locked} nesnesi")
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Ordered, A::Error> {
                let mut out = Vec::new();
                while let Some((term, entry)) = map.next_entry::<String, Entry>()? {
                    out.push((term, entry));
                }
                Ok(Ordered(out))
            }
        }

        deserializer.deserialize_map(OrderedVisitor)
    }
}

/// Python repr benzeri tek tırnaklı metin gösterimi (conflicts.log biçimi için).
fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    fn glossary() -> (tempfile::TempDir, Glossary) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("glossary.json");
        fs::write(
            &path,
            r#"{
  "Rover": {"tr": "Gezgin", "locked": false},
  "Rover Ascension": {"tr": "Gezgin Yükselişi", "locked": true},
  "Jiyan": {"tr": "Jiyan", "locked": true}
}"#,
        )
        .unwrap();
        let glossary = Glossary::open(&path).unwrap();
        (dir, glossary)
    }

    #[test]
    fn subset_is_case_insensitive_and_word_bounded() {
        let (_dir, g) = glossary();
        let subset = g.subset_for(["the rover arrives", "Roverish nonsense"]);
        assert_eq!(subset, pairs(&[("Rover", "Gezgin")]));
    }

    #[test]
    fn subset_prefers_longest_term() {
        let (_dir, g) = glossary();
        let subset = g.subset_for(["Rover Ascension starts now"]);
        assert_eq!(subset, pairs(&[("Rover Ascension", "Gezgin Yükselişi")]));
    }

    #[test]
    fn merge_new_first_decision_wins_and_logs_conflict() {
        let (dir, mut g) = glossary();
        let new_terms = BTreeMap::from([
            ("Rover".to_string(), "Seyyah".to_string()),
            ("Lupa".to_string(), "Lupa".to_string()),
        ]);
        let added = g.merge_new(&new_terms).unwrap().added;
        assert_eq!(added, vec!["Lupa".to_string()]);
        assert_eq!(g.get("Rover"), Some("Gezgin"));
        let log = fs::read_to_string(dir.path().join("conflicts.log")).unwrap();
        assert!(log.contains("Rover") && log.contains("Seyyah"));
    }

    #[test]
    fn locked_terms_never_change() {
        let (_dir, mut g) = glossary();
        g.merge_new(&BTreeMap::from([(
            "Jiyan".to_string(),
            "Ciyan".to_string(),
        )]))
        .unwrap();
        assert_eq!(g.get("Jiyan"), Some("Jiyan"));
        assert!(g.entries["Jiyan"].locked);
    }

    #[test]
    fn save_roundtrip_is_atomic() {
        let (dir, mut g) = glossary();
        g.merge_new(&BTreeMap::from([("Lupa".to_string(), "Lupa".to_string())]))
            .unwrap();
        let reloaded = Glossary::open(&dir.path().join("glossary.json")).unwrap();
        assert_eq!(reloaded.get("Lupa"), Some("Lupa"));
        assert!(!dir.path().join("glossary.json.tmp").exists());
    }

    #[test]
    fn lock_and_add() {
        let (_dir, mut g) = glossary();
        g.add("Yangyang", "Yangyang", false).unwrap();
        g.lock("Yangyang").unwrap();
        assert!(g.entries["Yangyang"].locked);
        assert!(g.lock("Yok").is_err());
    }

    #[test]
    fn empty_glossary_has_no_subset() {
        let dir = tempfile::tempdir().unwrap();
        let empty = Glossary::open(&dir.path().join("missing.json")).unwrap();
        assert!(empty.subset_for(["anything"]).is_empty());
        assert_eq!(empty.len(), 0);
    }

    #[test]
    fn remove_and_unlock_keep_order_and_lookup_consistent() {
        let (dir, mut g) = glossary();
        g.add("Alpha", "Alfa", true).unwrap();
        g.add("Beta", "Beta", false).unwrap();
        g.set_locked("Alpha", false).unwrap();
        assert!(!g.entries["Alpha"].locked);
        g.remove("alpha").unwrap();
        assert!(g.get("Alpha").is_none());
        assert!(g.remove("Alpha").is_err());
        let names: Vec<&str> = g.iter().map(|(term, _)| term).collect();
        assert!(!names.contains(&"Alpha"));
        let reloaded = Glossary::open(&dir.path().join("glossary.json")).unwrap();
        assert_eq!(reloaded.len(), g.len());
    }

    #[test]
    fn model_terms_are_marked_and_user_terms_are_not() {
        let (dir, mut g) = glossary();
        let proposed = BTreeMap::from([("Lupa".to_string(), "Lupa".to_string())]);
        g.merge_new(&proposed).unwrap();
        assert!(g.entries["Lupa"].auto);
        g.add("Lupa", "Lupa", false).unwrap();
        assert!(!g.entries["Lupa"].auto);
        let text = fs::read_to_string(dir.path().join("glossary.json")).unwrap();
        assert!(!text.contains("from_model"));
    }

    fn named(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn validator_accepts_names_and_rejects_sentences() {
        assert!(is_valid_term("Rover", "Gezgin"));
        assert!(is_valid_term("Jiyan", "Jiyan"));
        assert!(is_valid_term("Sword of Dawn", "Şafak Kılıcı"));
        assert!(!is_valid_term("The Rover walks home.", "Gezgin eve yürür."));
        assert!(!is_valid_term("Rover.", "Gezgin"));
        assert!(!is_valid_term("Rover…", "Gezgin"));
        assert!(!is_valid_term("Rover", "Gezgin: lider"));
        assert!(!is_valid_term("Rover", "Gezgin\nYeni satır"));
        assert!(!is_valid_term("Rover", "Gezgin!"));
        assert!(!is_valid_term("Bir Iki Uc Dort Bes", "Beş"));
        assert!(!is_valid_term(&"A".repeat(41), "Kısa"));
        assert!(!is_valid_term("Kısa", &"Ç".repeat(61)));
        assert!(!is_valid_term("  ", "Gezgin"));
        assert!(!is_valid_term("Rover", ""));
    }

    #[test]
    fn merge_new_skips_sentences_and_counts_them() {
        let (_dir, mut g) = glossary();
        let merged = g
            .merge_new(&named(&[
                ("Lupa", "Lupa"),
                ("Anxious Customer", "Anxious Customer"),
                ("A long sentence about the rover.", "Uzun bir cümle."),
            ]))
            .unwrap();
        assert_eq!(merged.added, vec!["Anxious Customer".to_string(), "Lupa".to_string()]);
        assert_eq!(merged.rejected, 1);
        assert!(g.get("A long sentence about the rover.").is_none());
    }

    #[test]
    fn add_rejects_invalid_term() {
        let (_dir, mut g) = glossary();
        assert!(g.add("Bu bir cümledir.", "Bu bir cümledir.", false).is_err());
        assert!(g.get("Bu bir cümledir.").is_none());
    }

    #[test]
    fn legacy_entries_load_with_auto_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("glossary.json");
        fs::write(
            &path,
            r#"{
  "Lupa": {"tr": "Lupa", "from_model": true},
  "Rover": {"tr": "Gezgin"},
  "Jiyan": {"tr": "Jiyan", "locked": true}
}"#,
        )
        .unwrap();
        let g = Glossary::open(&path).unwrap();
        assert!(g.entries["Lupa"].auto);
        assert!(g.entries["Rover"].auto);
        assert!(!g.entries["Jiyan"].auto);
        g.save().unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"auto\": true"));
        assert!(!text.contains("from_model"));
    }

    #[test]
    fn clean_dry_run_changes_nothing() {
        let (dir, mut g) = glossary();
        g.add("Sword of Dawn", "Şafak Kılıcı", false).unwrap();
        g.entries.get_mut("Rover").unwrap().tr = "Gezgin. Yeni.".into();
        let before = fs::read_to_string(dir.path().join("glossary.json")).unwrap();
        let report = g.clean(true).unwrap();
        assert_eq!(report.removed, vec!["Rover".to_string()]);
        assert_eq!(report.kept, 3);
        assert_eq!(g.get("Rover"), Some("Gezgin. Yeni."));
        assert_eq!(fs::read_to_string(dir.path().join("glossary.json")).unwrap(), before);
        let backups = fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains(".bak-"))
            .count();
        assert_eq!(backups, 0);
    }

    #[test]
    fn clean_removes_invalid_unlocked_and_keeps_locked() {
        let (dir, mut g) = glossary();
        g.add("Sword of Dawn", "Şafak Kılıcı", false).unwrap();
        g.entries.get_mut("Rover").unwrap().tr = "Gezgin. Yeni.".into();
        g.entries.get_mut("Jiyan").unwrap().tr = "Jiyan. Bu bir cümle.".into();
        let report = g.clean(false).unwrap();
        assert_eq!(report.removed, vec!["Rover".to_string()]);
        assert_eq!(report.locked_kept, 1);
        assert_eq!(report.kept, 3);
        assert!(g.get("Rover").is_none());
        assert!(g.get("Jiyan").is_some());
        let backups: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("glossary.json.bak-"))
            .collect();
        assert_eq!(backups.len(), 1);
        let reloaded = Glossary::open(&dir.path().join("glossary.json")).unwrap();
        assert!(reloaded.get("Rover").is_none());
        assert_eq!(reloaded.get("Jiyan"), Some("Jiyan. Bu bir cümle."));
        assert_eq!(reloaded.get("Sword of Dawn"), Some("Şafak Kılıcı"));
    }
}
