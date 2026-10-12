use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::glossary::TermMatcher;

/// Oyun şablonlarındaki etiketler, biçim belirteçleri ve köşeli parantez blokları.
pub const TAG_PATTERN: &str = concat!(
    r"<[^<>\n]*>",
    r"|\{[^{}\n]*\}",
    r"|%[-+ #0]*\d*(?:\.\d+)?[sdifu%]",
    r"|\\[nrt]",
    r"|\[[^\[\]\n]*\]",
);

static TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(TAG_PATTERN).expect("TAG_PATTERN"));
static TAG_AT_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("^(?:{TAG_PATTERN})")).expect("TAG_AT_START"));
static TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"⟦(\d+)⟧").expect("TOKEN_RE"));

/// (jeton, geri konacak metin) sıralı çifti.
pub type Mapping = Vec<(String, String)>;

pub fn protect(text: &str, glossary_subset: &[(String, String)]) -> (String, Mapping) {
    let matcher = TermMatcher::new(glossary_subset.iter().map(|(term, _)| term.as_str()));
    let replacements: HashMap<String, String> = glossary_subset
        .iter()
        .map(|(term, tr)| (term.to_lowercase(), tr.clone()))
        .collect();

    let chars: Vec<char> = text.chars().collect();
    let mut offsets: Vec<usize> = text.char_indices().map(|(byte, _)| byte).collect();
    offsets.push(text.len());

    let mut out = String::new();
    let mut mapping: Mapping = Vec::new();
    let mut last_byte = 0;
    let mut pos = 0;
    while pos < chars.len() {
        let start_byte = offsets[pos];
        let found = if let Some(tag) = TAG_AT_START.find(&text[start_byte..]) {
            let end_byte = start_byte + tag.end();
            let end = offsets.partition_point(|&byte| byte < end_byte);
            Some((end, tag.as_str().to_string()))
        } else {
            matcher.match_at(&chars, pos).map(|(end, _)| {
                let matched: String = chars[pos..end].iter().collect();
                let replacement = replacements
                    .get(&matched.to_lowercase())
                    .cloned()
                    .unwrap_or_else(|| matched.clone());
                (end, replacement)
            })
        };
        let Some((end, replacement)) = found else {
            pos += 1;
            continue;
        };
        let token = format!("⟦{}⟧", mapping.len());
        out.push_str(&text[last_byte..start_byte]);
        out.push_str(&token);
        mapping.push((token, replacement));
        last_byte = offsets[end];
        pos = end;
    }
    out.push_str(&text[last_byte..]);
    (out, mapping)
}

pub fn restore(masked: &str, mapping: &Mapping) -> String {
    let mut result = masked.to_string();
    for (token, value) in mapping {
        result = result.replace(token.as_str(), value);
    }
    result
}

/// Maskeli çeviride her jetonun tam olarak bir kez bulunup bulunmadığını denetler.
pub fn validate(translated: &str, mapping: &Mapping) -> bool {
    let mut found: HashMap<&str, usize> = HashMap::new();
    for caps in TOKEN_RE.captures_iter(translated) {
        if let Some(digits) = caps.get(1) {
            *found.entry(digits.as_str()).or_insert(0) += 1;
        }
    }
    let expected: HashSet<&str> = mapping
        .iter()
        .map(|(token, _)| token.trim_start_matches('⟦').trim_end_matches('⟧'))
        .collect();
    if found.keys().any(|key| !expected.contains(key)) {
        return false;
    }
    expected
        .iter()
        .all(|key| found.get(key).copied().unwrap_or(0) == 1)
}

pub fn placeholders_of(text: &str) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for found in TAG_RE.find_iter(text) {
        *counts.entry(found.as_str().to_string()).or_insert(0) += 1;
    }
    counts
}

/// Yapay zekâ çıktısı için: kaynak ve hedefteki yer tutucu çokluk kümeleri eşit mi?
pub fn validate_placeholders(source: &str, target: &str) -> bool {
    placeholders_of(source) == placeholders_of(target)
}

/// Yer tutucular çıkarıldığında harf kalıyor mu? Kalmıyorsa çeviri gerekmez.
pub fn has_words(text: &str) -> bool {
    TAG_RE
        .replace_all(text, " ")
        .chars()
        .any(char::is_alphabetic)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protect_restore_roundtrip_keeps_tags() {
        let text = r"<color=red>Rover</> has {0} items, %d left\n[Hint]";
        let subset = vec![("Rover".to_string(), "Gezgin".to_string())];
        let (masked, mapping) = protect(text, &subset);
        assert!(!masked.contains("<color=red>"));
        assert!(!masked.contains("Rover"));
        let restored = restore(&masked, &mapping);
        assert_eq!(
            restored,
            r"<color=red>Gezgin</> has {0} items, %d left\n[Hint]"
        );
    }

    #[test]
    fn validate_requires_each_token_exactly_once() {
        let (_masked, mapping) = protect("a {0} b {1}", &[]);
        let tokens: Vec<&str> = mapping.iter().map(|(token, _)| token.as_str()).collect();
        assert!(validate(
            &format!("x {} y {}", tokens[0], tokens[1]),
            &mapping
        ));
        assert!(!validate(&format!("x {} y", tokens[0]), &mapping));
        assert!(!validate(
            &format!("{0} {0} {1}", tokens[0], tokens[1]),
            &mapping
        ));
        assert!(!validate(
            &format!("{} {} ⟦99⟧", tokens[0], tokens[1]),
            &mapping
        ));
    }

    #[test]
    fn validate_placeholders_compares_multisets() {
        assert!(validate_placeholders(
            "Hi {0}, <b>%s</b>",
            "Merhaba {0}, <b>%s</b>"
        ));
        assert!(!validate_placeholders("Hi {0}", "Merhaba {1}"));
        assert!(!validate_placeholders("Hi {0} {0}", "Merhaba {0}"));
    }

    #[test]
    fn has_words_ignores_tags_numbers_and_symbols() {
        assert!(!has_words("<color=red></>"));
        assert!(!has_words("  123 -- {0} "));
        assert!(has_words("<b>Ok</b>"));
    }
}
