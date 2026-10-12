use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::engines::{BatchResult, EngineError, Item};
use crate::lang::Lang;

pub const SYSTEM_TEMPLATE: &str = r#"You translate in-game text of the video game Wuthering Waves from English into {target}.

Rules:
(a) Target language is {target}. Use natural, fluent game dialogue and UI tone. Keep it short and clear for UI labels.
(b) Tags and placeholders are never translated, removed, reordered or altered. This includes anything in angle brackets such as <color=...> or </>, braces such as {0} or {Name}, printf-style markers such as %s or %d, literal sequences such as \n, \r and \t, and square-bracket blocks such as [...]. Keep every line break inside the text.
(c) GLOSSARY lines (formatted EN=TR) are mandatory. Use exactly the given {english} form for those terms.
(d) Proper names that are NOT in the GLOSSARY are kept UNCHANGED. This covers characters, places, groups or organizations, items and weapons, attack or skill names, and game mechanic terms. Copy each such name exactly as written in the source, with the same letters, spelling and capitalization. Do not translate, transliterate, inflect or re-case it, and do not change a single letter.
(e) Descriptive titles and NPC names built from ordinary words (for example "Anxious Customer") are normal text: translate them like any other sentence and never report them in "new_terms".
(f) Report in "new_terms" only proper names or game terms that appear in the input texts and are not already in the GLOSSARY. Each key is the name exactly as written in the source and its value is the same name, unchanged. A term has at most 4 words. Never report sentences, phrases with punctuation, or glossary terms.
(g) Output ONLY one JSON object, with no commentary and no code fence:
{"t":{"1":"translation of item 1","2":"translation of item 2"},"new_terms":{"Name":"Name"}}
The keys of "t" are the short integer indexes from the input. Every input index must appear in "t".
(h) If an input is empty or cannot be translated (for example it only contains symbols or numbers), return it unchanged.
"#;

pub fn system_prompt(lang: &Lang) -> String {
    SYSTEM_TEMPLATE
        .replace("{target}", &lang.prompt_name())
        .replace("{english}", lang.english)
}

pub fn build_system(lang: &Lang, context: &str) -> String {
    let system = system_prompt(lang);
    if context.trim().is_empty() {
        return system;
    }
    format!("{system}\nPERSISTENT GLOSSARY (EN=TR):\n{context}\n")
}

/// Kullanıcı mesajı: partide geçen sözlük alt kümesi + indeksli kompakt JSON.
pub fn build_user(items: &[Item], glossary_subset: &[(String, String)]) -> String {
    let mut terms: Vec<&(String, String)> = glossary_subset.iter().collect();
    terms.sort_by_key(|(term, _)| term.to_lowercase());
    let mut lines = vec!["GLOSSARY:".to_string()];
    lines.extend(terms.iter().map(|(term, tr)| format!("{term}={tr}")));
    lines.push("TEXTS:".to_string());
    let pairs: Vec<String> = items
        .iter()
        .map(|item| {
            let key = serde_json::to_string(&item.key.to_string()).unwrap_or_default();
            let text = serde_json::to_string(&item.text).unwrap_or_default();
            format!("{key}:{text}")
        })
        .collect();
    lines.push(format!("{{{}}}", pairs.join(",")));
    lines.join("\n")
}

static FENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^```(?:json)?\s*|\s*```$").expect("FENCE"));

fn bad_shape(message: &str) -> EngineError {
    EngineError::Other(message.to_string())
}

/// Modelin çıktısını toleranslı biçimde JSON'a çevirir.
/// Kod bloğu, öncesinde/sonrasında metin varsa ilk '{' ile son '}' arası alınır.
pub fn parse_response(text: &str, items: &[Item]) -> Result<BatchResult, EngineError> {
    let cleaned = FENCE.replace_all(text, "");
    let cleaned = cleaned.trim();
    let (start, end) = match (cleaned.find('{'), cleaned.rfind('}')) {
        (Some(start), Some(end)) if end > start => (start, end),
        _ => return Err(bad_shape("Yanıtta JSON nesnesi bulunamadı.")),
    };
    let data: Value = serde_json::from_str(&cleaned[start..=end]).map_err(|error| {
        EngineError::Other(format!("Yanıt JSON olarak ayrıştırılamadı: {error}"))
    })?;
    let object = data
        .as_object()
        .ok_or_else(|| bad_shape("Yanıt bir JSON nesnesi değil."))?;

    let empty = serde_json::Map::new();
    let translated = match object.get("t") {
        Some(value) => value
            .as_object()
            .ok_or_else(|| bad_shape("\"t\" alanı bir nesne olmalı."))?,
        None if !object.contains_key("new_terms") => object,
        None => &empty,
    };
    let valid: HashSet<String> = items.iter().map(|item| item.key.to_string()).collect();
    let mut translations: BTreeMap<usize, String> = BTreeMap::new();
    for (key, value) in translated {
        if let (true, Some(text)) = (valid.contains(key), value.as_str()) {
            if let Ok(index) = key.parse::<usize>() {
                translations.insert(index, text.to_string());
            }
        }
    }

    let mut new_terms: BTreeMap<String, String> = BTreeMap::new();
    if let Some(Value::Object(raw)) = object.get("new_terms") {
        for (term, value) in raw {
            if let Some(tr) = value.as_str() {
                new_terms.insert(term.clone(), tr.to_string());
            }
        }
    }

    Ok(BatchResult {
        translations,
        new_terms,
        usage: BTreeMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang;

    fn items() -> Vec<Item> {
        vec![
            Item {
                key: 1,
                id: "a".into(),
                text: "Hello {0}".into(),
            },
            Item {
                key: 2,
                id: "b".into(),
                text: "Ş ok".into(),
            },
        ]
    }

    #[test]
    fn build_user_is_compact_and_unicode_safe() {
        let message = build_user(&items(), &[("Rover".to_string(), "Gezgin".to_string())]);
        assert!(message.contains("Rover=Gezgin"));
        let payload = message.lines().last().unwrap();
        let parsed: Value = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed, serde_json::json!({"1": "Hello {0}", "2": "Ş ok"}));
        assert!(!payload.contains("\": \"") && !payload.contains(", "));
        assert!(payload.contains('Ş'));
    }

    #[test]
    fn build_system_appends_persistent_glossary() {
        let tr = lang::validate("tr").unwrap();
        assert!(build_system(tr, "").starts_with("You translate"));
        assert!(build_system(tr, "Rover=Gezgin").contains("PERSISTENT GLOSSARY"));
    }

    #[test]
    fn turkish_prompt_keeps_original_wording() {
        let text = system_prompt(lang::validate("tr").unwrap());
        assert!(text.contains("from English into Turkish.\n"));
        assert!(text.contains("(a) Target language is Turkish. Use natural"));
        assert!(text.contains("Use exactly the given Turkish form for those terms."));
        assert!(text.contains(r#"{"Name":"Name"}"#));
        assert!(!text.contains("{target}") && !text.contains("{english}"));
    }

    #[test]
    fn prompt_keeps_names_unchanged_and_limits_new_terms() {
        for code in ["tr", "de"] {
            let text = system_prompt(lang::validate(code).unwrap());
            assert!(text.contains("kept UNCHANGED"));
            assert!(text.contains("exactly as written in the source"));
            assert!(text.contains("do not change a single letter"));
            assert!(text.contains("\"Anxious Customer\""));
            assert!(text.contains("at most 4 words"));
            assert!(text.contains("Never report sentences"));
            assert!(text.contains(r#"{"Name":"Name"}"#));
        }
    }

    #[test]
    fn other_language_prompt_names_target() {
        let de = system_prompt(lang::validate("de").unwrap());
        assert!(de.contains("from English into German (Deutsch)."));
        assert!(de.contains("Target language is German (Deutsch)."));
        assert!(de.contains(r#"{"Name":"Name"}"#));
        assert!(!de.contains("Turkish"));
        assert!(build_system(lang::validate("de").unwrap(), "Rover=Gezgin")
            .contains("PERSISTENT GLOSSARY"));
    }

    #[test]
    fn parse_plain_json() {
        let text = r#"{"t":{"1":"Merhaba {0}","2":"Ş tamam"},"new_terms":{"Lupa":"Lupa"}}"#;
        let result = parse_response(text, &items()).unwrap();
        assert_eq!(
            result.translations,
            BTreeMap::from([(1, "Merhaba {0}".into()), (2, "Ş tamam".into())])
        );
        assert_eq!(
            result.new_terms,
            BTreeMap::from([("Lupa".to_string(), "Lupa".to_string())])
        );
    }

    #[test]
    fn parse_tolerates_fences_and_surrounding_text() {
        let text = "Tabii! ```json\n{\"t\":{\"1\":\"Selam\"}}\n```\nİyi günler";
        let result = parse_response(text, &items()).unwrap();
        assert_eq!(result.translations, BTreeMap::from([(1, "Selam".into())]));
        assert!(result.new_terms.is_empty());
    }

    #[test]
    fn parse_ignores_unknown_indexes() {
        let result = parse_response(r#"{"t":{"1":"A","9":"B"}}"#, &items()).unwrap();
        assert_eq!(result.translations, BTreeMap::from([(1, "A".into())]));
    }

    #[test]
    fn parse_rejects_non_json() {
        assert!(parse_response("sadece metin", &items()).is_err());
        assert!(parse_response(r#"{"t": [1,2]}"#, &items()).is_err());
    }
}
