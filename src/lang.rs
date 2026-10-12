use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const DEFAULT_CODE: &str = "tr";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lang {
    pub code: &'static str,
    pub native: &'static str,
    pub english: &'static str,
    pub yandex: &'static str,
    pub google: &'static str,
}

pub const LANGS: &[Lang] = &[
    Lang { code: "tr", native: "Türkçe", english: "Turkish", yandex: "tr", google: "tr" },
    Lang { code: "ru", native: "Русский", english: "Russian", yandex: "ru", google: "ru" },
    Lang { code: "de", native: "Deutsch", english: "German", yandex: "de", google: "de" },
    Lang { code: "fr", native: "Français", english: "French", yandex: "fr", google: "fr" },
    Lang { code: "es", native: "Español", english: "Spanish", yandex: "es", google: "es" },
    Lang { code: "pt", native: "Português", english: "Portuguese", yandex: "pt", google: "pt" },
    Lang { code: "pt-BR", native: "Português do Brasil", english: "Brazilian Portuguese", yandex: "pt", google: "pt" },
    Lang { code: "it", native: "Italiano", english: "Italian", yandex: "it", google: "it" },
    Lang { code: "id", native: "Bahasa Indonesia", english: "Indonesian", yandex: "id", google: "id" },
    Lang { code: "ar", native: "العربية", english: "Arabic", yandex: "ar", google: "ar" },
    Lang { code: "ja", native: "日本語", english: "Japanese", yandex: "ja", google: "ja" },
    Lang { code: "ko", native: "한국어", english: "Korean", yandex: "ko", google: "ko" },
    Lang { code: "zh-CN", native: "简体中文", english: "Simplified Chinese", yandex: "zh", google: "zh-CN" },
    Lang { code: "pl", native: "Polski", english: "Polish", yandex: "pl", google: "pl" },
    Lang { code: "uk", native: "Українська", english: "Ukrainian", yandex: "uk", google: "uk" },
    Lang { code: "vi", native: "Tiếng Việt", english: "Vietnamese", yandex: "vi", google: "vi" },
    Lang { code: "th", native: "ไทย", english: "Thai", yandex: "th", google: "th" },
    Lang { code: "nl", native: "Nederlands", english: "Dutch", yandex: "nl", google: "nl" },
    Lang { code: "hi", native: "हिन्दी", english: "Hindi", yandex: "hi", google: "hi" },
];

pub fn find(code: &str) -> Option<&'static Lang> {
    let code = code.trim();
    LANGS
        .iter()
        .find(|lang| lang.code.eq_ignore_ascii_case(code))
}

pub fn validate(code: &str) -> Result<&'static Lang> {
    find(code).with_context(|| {
        format!(
            "Bilinmeyen hedef dil: {code}. Seçenekler: {}",
            LANGS.iter().map(|lang| lang.code).collect::<Vec<_>>().join(", ")
        )
    })
}

pub fn default_lang() -> &'static Lang {
    find(DEFAULT_CODE).expect("varsayılan dil tabloda")
}

impl Lang {
    pub fn is_default(&self) -> bool {
        self.code == DEFAULT_CODE
    }

    pub fn prompt_name(&self) -> String {
        if self.is_default() {
            self.english.to_string()
        } else {
            format!("{} ({})", self.english, self.native)
        }
    }

    pub fn tm_file(&self) -> String {
        if self.is_default() {
            "tm.db".into()
        } else {
            format!("tm.{}.db", self.code)
        }
    }

    pub fn glossary_file(&self) -> String {
        if self.is_default() {
            "glossary.json".into()
        } else {
            format!("glossary.{}.json", self.code)
        }
    }
}

pub fn tm_path(root: &Path, lang: &Lang) -> PathBuf {
    root.join(lang.tm_file())
}

pub fn glossary_path(root: &Path, lang: &Lang) -> PathBuf {
    root.join(lang.glossary_file())
}

pub fn pack_dir(root: &Path, lang: &Lang) -> PathBuf {
    root.join("packs").join(lang.code)
}

pub fn cache_dir(root: &Path, lang: &Lang) -> PathBuf {
    let base = root.join("packs").join(".cache");
    if lang.is_default() {
        base
    } else {
        base.join(lang.code)
    }
}

pub fn pack_url_for(template: &str, lang: &Lang) -> String {
    let url = template.trim();
    if url.contains("{lang}") {
        return url.replace("{lang}", lang.code);
    }
    match url.strip_suffix("/packs/tr") {
        Some(base) if !lang.is_default() => format!("{base}/packs/{}", lang.code),
        _ => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_and_default_exists() {
        let mut codes: Vec<&str> = LANGS.iter().map(|lang| lang.code).collect();
        let count = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), count);
        assert_eq!(default_lang().code, "tr");
        assert_eq!(LANGS.len(), 19);
        assert!(find("en").is_none());
    }

    #[test]
    fn validate_is_case_insensitive_and_canonical() {
        assert_eq!(validate(" pt-br ").unwrap().code, "pt-BR");
        assert_eq!(validate("ZH-cn").unwrap().code, "zh-CN");
        let error = validate("xx").unwrap_err().to_string();
        assert!(error.contains("xx") && error.contains("tr"));
    }

    #[test]
    fn storage_names_follow_language() {
        let root = Path::new("/kok");
        let tr = validate("tr").unwrap();
        let de = validate("de").unwrap();
        assert_eq!(tm_path(root, tr), Path::new("/kok/tm.db"));
        assert_eq!(tm_path(root, de), Path::new("/kok/tm.de.db"));
        assert_eq!(glossary_path(root, tr), Path::new("/kok/glossary.json"));
        assert_eq!(glossary_path(root, de), Path::new("/kok/glossary.de.json"));
        assert_eq!(pack_dir(root, tr), Path::new("/kok/packs/tr"));
        assert_eq!(pack_dir(root, de), Path::new("/kok/packs/de"));
        assert_eq!(cache_dir(root, tr), Path::new("/kok/packs/.cache"));
        assert_eq!(cache_dir(root, de), Path::new("/kok/packs/.cache/de"));
    }

    #[test]
    fn pack_url_placeholder_and_legacy_default() {
        let template = "https://h/main/packs/{lang}";
        let tr = validate("tr").unwrap();
        let ja = validate("ja").unwrap();
        assert_eq!(pack_url_for(template, tr), "https://h/main/packs/tr");
        assert_eq!(pack_url_for(template, ja), "https://h/main/packs/ja");
        let legacy = "https://h/main/packs/tr";
        assert_eq!(pack_url_for(legacy, tr), legacy);
        assert_eq!(pack_url_for(legacy, ja), "https://h/main/packs/ja");
        assert_eq!(pack_url_for("https://ornek.test/x", ja), "https://ornek.test/x");
    }

    #[test]
    fn prompt_name_keeps_turkish_plain() {
        assert_eq!(validate("tr").unwrap().prompt_name(), "Turkish");
        assert_eq!(validate("de").unwrap().prompt_name(), "German (Deutsch)");
    }
}
