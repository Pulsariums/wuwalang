use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::Serialize;

use crate::engines::{BatchResult, Engine, EngineError, Item};
use crate::glossary::Glossary;
use crate::placeholders::{has_words, protect, restore, validate, validate_placeholders, Mapping};
use crate::prompt::SYSTEM_TEMPLATE;
use crate::tm::TranslationMemory;
use crate::util::check_cancel;

/// (dosya, tablo, kimlik, kaynak metin)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub file: String,
    pub table: String,
    pub id: String,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct Batch {
    pub items: Vec<Item>,
    /// Ham kaynak metinler (items ile aynı sırada).
    pub sources: Vec<String>,
    /// Her kaynağın oyundaki tüm satır kimlikleri.
    pub rids: Vec<Vec<String>>,
    /// MT için yer tutucu eşlemeleri (AI'da boş).
    pub maps: Vec<Mapping>,
    pub table: String,
    pub chars: usize,
}

impl Batch {
    fn new(table: String) -> Self {
        Self {
            items: Vec::new(),
            sources: Vec::new(),
            rids: Vec::new(),
            maps: Vec::new(),
            table,
            chars: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub batches: Vec<Batch>,
    pub tm_hits: HashMap<String, String>,
    /// Çevrilmeden aynen kalacak satır sayısı.
    pub copied: usize,
    pub total_rows: usize,
    pub unique_translatable: usize,
    /// TM'de olmayan benzersiz çevrilebilir metin.
    pub unique_pending: usize,
}

pub fn is_translatable(source: &str) -> bool {
    !source.trim().is_empty() && has_words(source)
}

/// Kayıtlardan çeviri planı üretir: TM isabeti, tekilleştirme, tablo bazlı paketleme.
pub fn plan(
    records: &[Record],
    tm: &TranslationMemory,
    glossary: &Glossary,
    engine: &dyn Engine,
    max_items: usize,
    max_chars: usize,
) -> Result<Plan> {
    let mut sorted: Vec<&Record> = records.iter().collect();
    sorted.sort_by(|a, b| {
        (a.table.as_str(), a.file.as_str()).cmp(&(b.table.as_str(), b.file.as_str()))
    });

    let mut sources: Vec<String> = Vec::new();
    let mut rids: HashMap<String, Vec<String>> = HashMap::new();
    let mut table_of: HashMap<String, String> = HashMap::new();
    let mut copied = 0;
    for record in sorted {
        if !is_translatable(&record.source) {
            copied += 1;
            continue;
        }
        let rid = format!("{}:{}:{}", record.file, record.table, record.id);
        match rids.get_mut(&record.source) {
            Some(list) => list.push(rid),
            None => {
                sources.push(record.source.clone());
                rids.insert(record.source.clone(), vec![rid]);
                table_of.insert(record.source.clone(), record.table.clone());
            }
        }
    }

    let hits = tm.get_many(&sources)?;
    let pending: Vec<&String> = sources
        .iter()
        .filter(|source| !hits.contains_key(*source))
        .collect();

    let limit_items = max_items.min(engine.max_items()).max(1);
    let limit_chars = max_chars.min(engine.max_batch_chars()).max(1);
    let mut batches: Vec<Batch> = Vec::new();
    let mut current: Option<Batch> = None;
    for source in &pending {
        let table = table_of[*source].clone();
        let (text, mapping) = if engine.machine_translation() {
            protect(source, &glossary.subset_for([source.as_str()]))
        } else {
            (source.to_string(), Vec::new())
        };
        let length = text.chars().count();
        let full = match &current {
            None => true,
            Some(batch) => {
                batch.table != table
                    || batch.items.len() >= limit_items
                    || (!batch.items.is_empty() && batch.chars + length > limit_chars)
            }
        };
        if full {
            if let Some(done) = current.take() {
                batches.push(done);
            }
            current = Some(Batch::new(table));
        }
        if let Some(batch) = current.as_mut() {
            batch.items.push(Item {
                key: batch.items.len() + 1,
                id: rids[*source][0].clone(),
                text,
            });
            batch.sources.push(source.to_string());
            batch.rids.push(rids[*source].clone());
            batch.maps.push(mapping);
            batch.chars += length;
        }
    }
    if let Some(done) = current {
        batches.push(done);
    }

    Ok(Plan {
        batches,
        tm_hits: hits,
        copied,
        total_rows: records.len(),
        unique_translatable: sources.len(),
        unique_pending: pending.len(),
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DryRun {
    pub batches: usize,
    pub chars: usize,
    pub rows: usize,
    pub copied_rows: usize,
    pub unique_translatable: usize,
    pub tm_hits: usize,
    pub tm_hit_ratio: f64,
    pub unique_pending: usize,
    pub estimated_input_tokens: usize,
    pub estimated_output_tokens: usize,
    pub note: &'static str,
}

/// API çağırmadan tahmin: paket sayısı, karakter, TM isabet oranı, kaba token tahmini.
pub fn dry_run(plan: &Plan, engine: &dyn Engine) -> DryRun {
    let chars: usize = plan.batches.iter().map(|batch| batch.chars).sum();
    let ratio = if plan.unique_translatable > 0 {
        plan.tm_hits.len() as f64 / plan.unique_translatable as f64
    } else {
        1.0
    };
    let (estimated_input_tokens, estimated_output_tokens) = if engine.machine_translation() {
        (0, 0)
    } else {
        // Kaba tahmin: ~3 karakter = 1 token; sistem istemi her pakette yeniden gönderilir.
        let system_tokens = SYSTEM_TEMPLATE.chars().count() / 3;
        (chars / 3 + plan.batches.len() * system_tokens, chars / 3)
    };
    DryRun {
        batches: plan.batches.len(),
        chars,
        rows: plan.total_rows,
        copied_rows: plan.copied,
        unique_translatable: plan.unique_translatable,
        tm_hits: plan.tm_hits.len(),
        tm_hit_ratio: (ratio * 10000.0).round() / 10000.0,
        unique_pending: plan.unique_pending,
        estimated_input_tokens,
        estimated_output_tokens,
        note: "Token sayısı kabaca tahmindir (≈3 karakter/token); makine çevirisinde token yoktur.",
    }
}

/// Üstel geri çekilme. 429/5xx ve ağ hatalarında yeniden dener; Retry-After'a uyar.
pub fn call_with_retry<F>(
    call: F,
    attempts: u32,
    sleep: &(dyn Fn(Duration) + Sync),
    log: &(dyn Fn(&str) + Sync),
) -> Result<BatchResult, EngineError>
where
    F: FnMut() -> Result<BatchResult, EngineError>,
{
    call_with_retry_cancellable(call, attempts, sleep, log, None)
}

fn is_cancelled(flag: Option<&AtomicBool>) -> bool {
    check_cancel(flag).is_err()
}

fn cancelled_error() -> EngineError {
    EngineError::Other(CANCELLED_MESSAGE.into())
}

fn call_with_retry_cancellable<F>(
    mut call: F,
    attempts: u32,
    sleep: &(dyn Fn(Duration) + Sync),
    log: &(dyn Fn(&str) + Sync),
    cancelled: Option<&AtomicBool>,
) -> Result<BatchResult, EngineError>
where
    F: FnMut() -> Result<BatchResult, EngineError>,
{
    for attempt in 1..=attempts {
        if is_cancelled(cancelled) {
            return Err(cancelled_error());
        }
        let error = match call() {
            Ok(result) => return Ok(result),
            Err(error) => error,
        };
        if is_fatal(&error) {
            return Err(error);
        }
        let retry_after = match &error {
            EngineError::Api { retry_after, .. } => *retry_after,
            _ => None,
        };
        if attempt == attempts {
            return Err(error);
        }
        let wait = retry_after
            .filter(|w| w.is_finite() && *w >= 0.0)
            .map(|w| w.min(120.0))
            .unwrap_or_else(|| 60f64.min(2f64.powi(attempt as i32)));
        log(&format!(
            "Geçici hata, {wait:.0} sn sonra yeniden denenecek ({attempt}/{attempts})."
        ));
        let wait = Duration::from_secs_f64(wait);
        if cancelled.is_none() {
            sleep(wait);
            continue;
        }
        let mut remaining = wait;
        while !remaining.is_zero() {
            if is_cancelled(cancelled) {
                return Err(cancelled_error());
            }
            let step = remaining.min(CANCEL_STEP);
            sleep(step);
            remaining -= step;
        }
    }
    Err(EngineError::Other(
        "Yeniden deneme sınırına ulaşıldı.".into(),
    ))
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RunSummary {
    pub batches: usize,
    pub tm_hits: usize,
    pub copied: usize,
    pub translated: usize,
    pub retry_ok: usize,
    pub failed: usize,
    pub terms_added: usize,
    pub terms_rejected: usize,
    pub usage: BTreeMap<String, u64>,
}

pub struct RunOptions<'a> {
    pub threads: usize,
    pub cancelled: Option<&'a AtomicBool>,
    pub log: &'a (dyn Fn(&str) + Sync),
    pub sleep: &'a (dyn Fn(Duration) + Sync),
    pub attempts: u32,
}

type Job = (usize, Vec<Item>, Vec<(String, String)>, String);

fn make_job(index: usize, batch: &Batch, glossary: &Glossary, engine: &dyn Engine) -> Job {
    let (subset, context) = if engine.machine_translation() {
        (Vec::new(), String::new())
    } else {
        (glossary.subset_for(&batch.sources), glossary.lines())
    };
    (index, batch.items.clone(), subset, context)
}

const CANCEL_POLL: Duration = Duration::from_millis(100);
const CANCEL_STEP: Duration = Duration::from_millis(200);
const CANCELLED_MESSAGE: &str = "İşlem iptal edildi.";

#[derive(Default)]
struct RunState {
    summary: RunSummary,
    retry_queue: Vec<(usize, usize)>,
}

/// Sonucu doğrular, TM'e yazar; doğrulanamayan öğelerin indekslerini döndürür.
fn apply(
    batch: &Batch,
    result: &BatchResult,
    indexes: &[usize],
    engine: &dyn Engine,
    glossary: &mut Glossary,
    tm: &TranslationMemory,
    state: &mut RunState,
) -> Result<Vec<usize>> {
    if !engine.machine_translation() && !result.new_terms.is_empty() {
        let merged = glossary.merge_new(&result.new_terms)?;
        state.summary.terms_added += merged.added.len();
        state.summary.terms_rejected += merged.rejected;
    }
    let mut rows: Vec<(String, String, String)> = Vec::new();
    let mut invalid = Vec::new();
    for &idx in indexes {
        let source = &batch.sources[idx];
        let mapping = &batch.maps[idx];
        let out = match result.translations.get(&batch.items[idx].key) {
            Some(out) if !out.trim().is_empty() => out,
            _ => {
                invalid.push(idx);
                continue;
            }
        };
        let final_text = if engine.machine_translation() {
            if !validate(out, mapping) {
                invalid.push(idx);
                continue;
            }
            restore(out, mapping)
        } else {
            if !validate_placeholders(source, out) {
                invalid.push(idx);
                continue;
            }
            out.clone()
        };
        rows.push((source.clone(), final_text, engine.name().to_string()));
    }
    tm.put_many(&rows)?;
    state.summary.translated += rows.len();
    for (key, value) in &result.usage {
        *state.summary.usage.entry(key.clone()).or_insert(0) += value;
    }
    Ok(invalid)
}

fn is_fatal(error: &EngineError) -> bool {
    matches!(error, EngineError::Api { .. }) && !error.is_retryable_api()
}

/// Paketleri çalıştırır. Her paketten sonra TM ve sözlük kaydedilir (ilerleme korunur).
pub fn run(
    plan: Plan,
    engine: &dyn Engine,
    tm: &TranslationMemory,
    glossary: &mut Glossary,
    opts: &RunOptions<'_>,
) -> Result<RunSummary> {
    let Plan {
        batches,
        tm_hits,
        copied,
        ..
    } = plan;
    let total = batches.len();
    let threads = opts.threads.max(1);
    let mut state = RunState {
        summary: RunSummary {
            batches: total,
            tm_hits: tm_hits.len(),
            copied,
            ..RunSummary::default()
        },
        retry_queue: Vec::new(),
    };

    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let job_rx = Mutex::new(job_rx);
    let (res_tx, res_rx) = mpsc::channel::<(usize, Result<BatchResult, EngineError>)>();

    std::thread::scope(|scope| -> Result<()> {
        for _ in 0..threads {
            let job_rx = &job_rx;
            let res_tx = res_tx.clone();
            scope.spawn(move || loop {
                let job = match job_rx.lock() {
                    Ok(guard) => guard.recv(),
                    Err(_) => break,
                };
                let Ok((index, items, subset, context)) = job else {
                    break;
                };
                let outcome = call_with_retry_cancellable(
                    || engine.translate_batch(&items, &subset, &context),
                    opts.attempts,
                    opts.sleep,
                    opts.log,
                    opts.cancelled,
                );
                if res_tx.send((index, outcome)).is_err() {
                    break;
                }
            });
        }
        drop(res_tx);

        let mut next = 0;
        let mut in_flight = 0;
        let mut done = 0;
        let mut stopped = is_cancelled(opts.cancelled);
        while !stopped && next < total && in_flight < threads {
            job_tx.send(make_job(next, &batches[next], glossary, engine))?;
            next += 1;
            in_flight += 1;
        }
        while in_flight > 0 {
            if is_cancelled(opts.cancelled) {
                stopped = true;
            }
            let (index, outcome) = match res_rx.recv_timeout(CANCEL_POLL) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(anyhow!("çalışan iş parçacıkları kapandı"));
                }
            };
            in_flight -= 1;
            let batch = &batches[index];
            let all: Vec<usize> = (0..batch.items.len()).collect();
            match outcome {
                Ok(result) => {
                    for bad in apply(batch, &result, &all, engine, glossary, tm, &mut state)? {
                        state.retry_queue.push((index, bad));
                    }
                }
                Err(error) if is_fatal(&error) => return Err(error.into()),
                Err(_) if stopped => {}
                Err(error) => {
                    state.summary.failed += batch.items.len();
                    (opts.log)(&format!("Paket başarısız: {error}"));
                }
            }
            done += 1;
            (opts.log)(&format!("Paket {done}/{total} tamamlandı."));
            if !stopped && next < total {
                job_tx.send(make_job(next, &batches[next], glossary, engine))?;
                next += 1;
                in_flight += 1;
            }
        }
        drop(job_tx);
        Ok(())
    })?;
    check_cancel(opts.cancelled)?;
    if state.summary.terms_rejected > 0 {
        (opts.log)(&format!(
            "{} geçersiz terim atlandı (cümle veya uzun metin).",
            state.summary.terms_rejected
        ));
    }

    // Doğrulanamayan öğeler tek tek bir kez daha denenir.
    for (index, bad) in std::mem::take(&mut state.retry_queue) {
        check_cancel(opts.cancelled)?;
        let batch = &batches[index];
        let single = Batch {
            items: vec![Item {
                key: 1,
                id: batch.items[bad].id.clone(),
                text: batch.items[bad].text.clone(),
            }],
            sources: vec![batch.sources[bad].clone()],
            rids: vec![batch.rids[bad].clone()],
            maps: vec![batch.maps[bad].clone()],
            table: batch.table.clone(),
            chars: batch.items[bad].text.chars().count(),
        };
        let (_, items, subset, context) = make_job(0, &single, glossary, engine);
        let outcome = call_with_retry_cancellable(
            || engine.translate_batch(&items, &subset, &context),
            opts.attempts,
            opts.sleep,
            opts.log,
            opts.cancelled,
        );
        match outcome {
            Ok(result) => {
                let invalid = apply(&single, &result, &[0], engine, glossary, tm, &mut state)?;
                if invalid.is_empty() {
                    state.summary.retry_ok += 1;
                } else {
                    state.summary.failed += 1;
                }
            }
            Err(error) if is_fatal(&error) => return Err(error.into()),
            Err(_) => state.summary.failed += 1,
        }
    }

    Ok(state.summary)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;
    use crate::util::Cancelled;

    type Reply = Box<dyn Fn(&[Item]) -> Result<BatchResult, EngineError> + Send + Sync>;
    type Call = (Vec<Item>, Vec<(String, String)>, String);

    struct FakeAi {
        reply: Option<Reply>,
        max_chars: usize,
        calls: Mutex<Vec<Call>>,
    }

    impl FakeAi {
        fn new() -> Self {
            Self {
                reply: None,
                max_chars: 6000,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl Engine for FakeAi {
        fn name(&self) -> &str {
            "fake-ai"
        }

        fn machine_translation(&self) -> bool {
            false
        }

        fn max_batch_chars(&self) -> usize {
            self.max_chars
        }

        fn translate_batch(
            &self,
            items: &[Item],
            glossary: &[(String, String)],
            context: &str,
        ) -> Result<BatchResult, EngineError> {
            self.calls.lock().unwrap().push((
                items.to_vec(),
                glossary.to_vec(),
                context.to_string(),
            ));
            match &self.reply {
                Some(reply) => reply(items),
                None => Ok(BatchResult {
                    translations: items
                        .iter()
                        .map(|i| (i.key, format!("TR:{}", i.text)))
                        .collect(),
                    ..BatchResult::default()
                }),
            }
        }
    }

    struct FakeMt;

    impl Engine for FakeMt {
        fn name(&self) -> &str {
            "fake-mt"
        }

        fn machine_translation(&self) -> bool {
            true
        }

        fn max_batch_chars(&self) -> usize {
            9000
        }

        fn translate_batch(
            &self,
            items: &[Item],
            _glossary: &[(String, String)],
            _context: &str,
        ) -> Result<BatchResult, EngineError> {
            Ok(BatchResult {
                translations: items
                    .iter()
                    .map(|i| (i.key, i.text.replace("Hello", "Merhaba")))
                    .collect(),
                ..BatchResult::default()
            })
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        tm: TranslationMemory,
        glossary: Glossary,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let tm = TranslationMemory::open(&dir.path().join("tm.db")).unwrap();
        let glossary = Glossary::open(&dir.path().join("glossary.json")).unwrap();
        Fixture {
            _dir: dir,
            tm,
            glossary,
        }
    }

    fn rec(table: &str, ident: usize, source: &str) -> Record {
        rec_in(table, ident, source, "a.db")
    }

    fn rec_in(table: &str, ident: usize, source: &str, file: &str) -> Record {
        Record {
            file: file.into(),
            table: table.into(),
            id: ident.to_string(),
            source: source.into(),
        }
    }

    fn noop_log(_message: &str) {}

    fn noop_sleep(_duration: Duration) {}

    fn quiet_opts(threads: usize, attempts: u32) -> RunOptions<'static> {
        RunOptions {
            threads,
            cancelled: None,
            log: &noop_log,
            sleep: &noop_sleep,
            attempts,
        }
    }

    #[test]
    fn dedupe_same_source_across_rows() {
        let f = fixture();
        let records = vec![
            rec("T", 1, "Hello world"),
            rec_in("U", 2, "Hello world", "b.db"),
        ];
        let p = plan(&records, &f.tm, &f.glossary, &FakeAi::new(), 60, 6000).unwrap();
        assert_eq!(p.unique_translatable, 1);
        assert_eq!(p.batches.len(), 1);
        assert_eq!(
            p.batches[0].rids,
            vec![vec!["a.db:T:1".to_string(), "b.db:U:2".to_string()]]
        );
    }

    #[test]
    fn tm_hits_are_removed_from_batches() {
        let f = fixture();
        f.tm.put_many(&[("Hello world", "Merhaba dünya", "x")])
            .unwrap();
        let records = vec![rec("T", 1, "Hello world"), rec("T", 2, "Bye now")];
        let p = plan(&records, &f.tm, &f.glossary, &FakeAi::new(), 60, 6000).unwrap();
        assert_eq!(
            p.tm_hits,
            HashMap::from([("Hello world".to_string(), "Merhaba dünya".to_string())])
        );
        let texts: Vec<&str> = p
            .batches
            .iter()
            .flat_map(|b| b.items.iter().map(|i| i.text.as_str()))
            .collect();
        assert_eq!(texts, vec!["Bye now"]);
    }

    #[test]
    fn untranslatable_rows_are_copied() {
        let f = fixture();
        let records = vec![
            rec("T", 1, "123"),
            rec("T", 2, "   "),
            rec("T", 3, "<color=red></>"),
            rec("T", 4, "Word"),
        ];
        let p = plan(&records, &f.tm, &f.glossary, &FakeAi::new(), 60, 6000).unwrap();
        assert_eq!(p.copied, 3);
        assert_eq!(p.unique_translatable, 1);
    }

    #[test]
    fn batch_item_limit_and_index_mapping() {
        let f = fixture();
        let records: Vec<Record> = (0..5).map(|i| rec("T", i, &format!("text {i}"))).collect();
        let p = plan(&records, &f.tm, &f.glossary, &FakeAi::new(), 2, 6000).unwrap();
        let sizes: Vec<usize> = p.batches.iter().map(|b| b.items.len()).collect();
        assert_eq!(sizes, vec![2, 2, 1]);
        for b in &p.batches {
            let keys: Vec<usize> = b.items.iter().map(|i| i.key).collect();
            assert_eq!(keys, (1..=b.items.len()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn batch_char_limit() {
        let f = fixture();
        let records: Vec<Record> = (0..4)
            .map(|i| rec("T", i, &format!("{}{i}", "x".repeat(39))))
            .collect();
        let p = plan(&records, &f.tm, &f.glossary, &FakeAi::new(), 60, 100).unwrap();
        assert!(p.batches.iter().all(|b| b.chars <= 100));
        assert_eq!(p.batches.len(), 2);
    }

    #[test]
    fn batches_are_grouped_by_table() {
        let f = fixture();
        let records = vec![rec("A", 1, "one"), rec("B", 2, "two"), rec("A", 3, "three")];
        let p = plan(&records, &f.tm, &f.glossary, &FakeAi::new(), 60, 6000).unwrap();
        let tables: Vec<&str> = p.batches.iter().map(|b| b.table.as_str()).collect();
        assert_eq!(tables, vec!["A", "B"]);
    }

    #[test]
    fn run_stores_tm_and_merges_new_terms() {
        let mut f = fixture();
        let mut engine = FakeAi::new();
        engine.reply = Some(Box::new(|items: &[Item]| {
            Ok(BatchResult {
                translations: items
                    .iter()
                    .map(|i| (i.key, format!("TR:{}", i.text)))
                    .collect(),
                new_terms: BTreeMap::from([("Rover".to_string(), "Gezgin".to_string())]),
                ..BatchResult::default()
            })
        }));
        let records = vec![rec("T", 1, "Rover waits"), rec("T", 2, "Other")];
        let p = plan(&records, &f.tm, &f.glossary, &engine, 60, 6000).unwrap();
        let summary = run(p, &engine, &f.tm, &mut f.glossary, &quiet_opts(1, 5)).unwrap();
        assert_eq!(summary.translated, 2);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.terms_added, 1);
        assert_eq!(f.glossary.get("Rover"), Some("Gezgin"));
        assert_eq!(
            f.tm.get_many(&["Rover waits"]).unwrap()["Rover waits"],
            "TR:Rover waits"
        );
    }

    #[test]
    fn manual_entries_are_hits_and_never_retranslated() {
        let f = fixture();
        f.tm.set_manual("Rover waits", "Elle düzeltildi").unwrap();
        let records = vec![rec("T", 1, "Rover waits"), rec("T", 2, "Other")];
        let p = plan(&records, &f.tm, &f.glossary, &FakeAi::new(), 60, 6000).unwrap();
        assert_eq!(p.tm_hits["Rover waits"], "Elle düzeltildi");
        let pending: Vec<&str> = p
            .batches
            .iter()
            .flat_map(|b| b.sources.iter().map(String::as_str))
            .collect();
        assert_eq!(pending, vec!["Other"]);
    }

    #[test]
    fn run_passes_glossary_subset_and_context() {
        let mut f = fixture();
        f.glossary.add("Rover", "Gezgin", true).unwrap();
        let engine = FakeAi::new();
        let p = plan(
            &[rec("T", 1, "The Rover moves")],
            &f.tm,
            &f.glossary,
            &engine,
            60,
            6000,
        )
        .unwrap();
        run(p, &engine, &f.tm, &mut f.glossary, &quiet_opts(1, 5)).unwrap();
        let calls = engine.calls.lock().unwrap();
        assert_eq!(
            calls[0].1,
            vec![("Rover".to_string(), "Gezgin".to_string())]
        );
        assert!(calls[0].2.contains("Rover=Gezgin"));
    }

    #[test]
    fn invalid_placeholders_are_retried_once_then_counted() {
        let mut f = fixture();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let mut engine = FakeAi::new();
        engine.reply = Some(Box::new(move |items: &[Item]| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(BatchResult {
                translations: items
                    .iter()
                    .map(|i| (i.key, "Bozuk çıktı".to_string()))
                    .collect(),
                ..BatchResult::default()
            })
        }));
        let p = plan(
            &[rec("T", 1, "Hi {0}")],
            &f.tm,
            &f.glossary,
            &engine,
            60,
            6000,
        )
        .unwrap();
        let summary = run(p, &engine, &f.tm, &mut f.glossary, &quiet_opts(1, 5)).unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(summary.failed, 1);
        assert_eq!(f.tm.count().unwrap(), 0);
    }

    #[test]
    fn machine_translation_restores_placeholders() {
        let mut f = fixture();
        f.glossary.add("Rover", "Gezgin", true).unwrap();
        let engine = FakeMt;
        let p = plan(
            &[rec("T", 1, "<b>Hello</b> Rover {0}")],
            &f.tm,
            &f.glossary,
            &engine,
            60,
            9000,
        )
        .unwrap();
        assert!(p.batches[0].items[0].text.contains('⟦'));
        let summary = run(p, &engine, &f.tm, &mut f.glossary, &quiet_opts(1, 5)).unwrap();
        assert_eq!(summary.translated, 1);
        assert_eq!(
            f.tm.get_many(&["<b>Hello</b> Rover {0}"]).unwrap()["<b>Hello</b> Rover {0}"],
            "<b>Merhaba</b> Gezgin {0}"
        );
    }

    #[test]
    fn dry_run_reports_estimates() {
        let f = fixture();
        f.tm.put_many(&[("Cached", "Önbellek", "x")]).unwrap();
        let engine = FakeAi::new();
        let p = plan(
            &[rec("T", 1, "Cached"), rec("T", 2, "Fresh text")],
            &f.tm,
            &f.glossary,
            &engine,
            60,
            6000,
        )
        .unwrap();
        let info = dry_run(&p, &engine);
        assert_eq!(info.tm_hits, 1);
        assert_eq!(info.tm_hit_ratio, 0.5);
        assert_eq!(info.batches, 1);
        assert_eq!(info.chars, "Fresh text".len());
    }

    #[test]
    fn call_with_retry_honours_retry_after_and_backoff() {
        let waits = Mutex::new(Vec::new());
        let sleep = |d: Duration| waits.lock().unwrap().push(d.as_secs_f64());
        let mut outcomes = VecDeque::from(vec![
            Err(EngineError::Api {
                status: 429,
                message: "busy".into(),
                retry_after: Some(3.0),
            }),
            Err(EngineError::Api {
                status: 503,
                message: "down".into(),
                retry_after: None,
            }),
            Ok(BatchResult::default()),
        ]);
        let result = call_with_retry(|| outcomes.pop_front().unwrap(), 5, &sleep, &|_: &str| {});
        assert!(result.is_ok());
        assert_eq!(*waits.lock().unwrap(), vec![3.0, 4.0]);
    }

    #[test]
    fn call_with_retry_stops_on_non_retryable_error() {
        let calls = AtomicUsize::new(0);
        let result = call_with_retry(
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(EngineError::Api {
                    status: 401,
                    message: "bad key".into(),
                    retry_after: None,
                })
            },
            5,
            &|_: Duration| {},
            &|_: &str| {},
        );
        assert!(matches!(result, Err(EngineError::Api { status: 401, .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn call_with_retry_gives_up_after_attempts() {
        let result = call_with_retry(
            || Err(EngineError::Other("bozuk yanıt".into())),
            2,
            &|_: Duration| {},
            &|_: &str| {},
        );
        assert!(matches!(result, Err(EngineError::Other(_))));
    }

    #[test]
    fn cancel_before_run_calls_nothing() {
        let mut f = fixture();
        let engine = FakeAi::new();
        let p = plan(
            &[rec("T", 1, "Hello")],
            &f.tm,
            &f.glossary,
            &engine,
            60,
            6000,
        )
        .unwrap();
        let flag = AtomicBool::new(true);
        let opts = RunOptions {
            threads: 2,
            cancelled: Some(&flag),
            log: &noop_log,
            sleep: &noop_sleep,
            attempts: 3,
        };
        let error = run(p, &engine, &f.tm, &mut f.glossary, &opts).unwrap_err();
        assert!(error.downcast_ref::<Cancelled>().is_some());
        assert!(engine.calls.lock().unwrap().is_empty());
        assert_eq!(f.tm.count().unwrap(), 0);
    }

    #[test]
    fn cancel_finishes_in_flight_batch_and_stops_scheduling() {
        let mut f = fixture();
        let flag = Arc::new(AtomicBool::new(false));
        let trigger = Arc::clone(&flag);
        let mut engine = FakeAi::new();
        engine.reply = Some(Box::new(move |items: &[Item]| {
            trigger.store(true, Ordering::SeqCst);
            Ok(BatchResult {
                translations: items
                    .iter()
                    .map(|i| (i.key, format!("TR:{}", i.text)))
                    .collect(),
                ..BatchResult::default()
            })
        }));
        let records: Vec<Record> = (0..4).map(|i| rec("T", i, &format!("text {i}"))).collect();
        let p = plan(&records, &f.tm, &f.glossary, &engine, 1, 6000).unwrap();
        let opts = RunOptions {
            threads: 1,
            cancelled: Some(flag.as_ref()),
            log: &noop_log,
            sleep: &noop_sleep,
            attempts: 3,
        };
        let error = run(p, &engine, &f.tm, &mut f.glossary, &opts).unwrap_err();
        assert!(error.downcast_ref::<Cancelled>().is_some());
        assert_eq!(engine.calls.lock().unwrap().len(), 1);
        assert_eq!(f.tm.count().unwrap(), 1);
    }

    #[test]
    fn retry_backoff_stops_when_cancelled() {
        let flag = AtomicBool::new(false);
        let calls = AtomicUsize::new(0);
        let result = call_with_retry_cancellable(
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                flag.store(true, Ordering::SeqCst);
                Err(EngineError::Transport("ağ".into()))
            },
            5,
            &|_: Duration| {},
            &|_: &str| {},
            Some(&flag),
        );
        assert!(matches!(result, Err(EngineError::Other(message)) if message == CANCELLED_MESSAGE));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn run_aborts_on_non_retryable_api_error() {
        let mut f = fixture();
        let mut engine = FakeAi::new();
        engine.reply = Some(Box::new(|_: &[Item]| {
            Err(EngineError::Api {
                status: 401,
                message: "bad key".into(),
                retry_after: None,
            })
        }));
        let p = plan(
            &[rec("T", 1, "Hello")],
            &f.tm,
            &f.glossary,
            &engine,
            60,
            6000,
        )
        .unwrap();
        let err = run(p, &engine, &f.tm, &mut f.glossary, &quiet_opts(1, 5)).unwrap_err();
        assert!(matches!(
            err.downcast_ref::<EngineError>(),
            Some(EngineError::Api { status: 401, .. })
        ));
    }
}
