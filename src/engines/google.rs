use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{json, Value};

use super::{check_response, BatchResult, Engine, EngineError, HttpClient, Item};
use crate::lang;

pub const URL: &str = "https://translation.googleapis.com/language/translate/v2";

pub struct GoogleEngine {
    http: Arc<dyn HttpClient>,
    api_key: String,
    target: &'static str,
}

impl GoogleEngine {
    pub fn new(http: Arc<dyn HttpClient>, api_key: &str, target: &str) -> Result<Self, EngineError> {
        if api_key.is_empty() {
            return Err(EngineError::Other("Google için api_key gerekli.".into()));
        }
        Ok(Self {
            http,
            api_key: api_key.to_string(),
            target: lang::validate(target)
                .map_err(|error| EngineError::Other(error.to_string()))?
                .google,
        })
    }
}

impl Engine for GoogleEngine {
    fn name(&self) -> &str {
        "google"
    }

    fn machine_translation(&self) -> bool {
        true
    }

    fn max_items(&self) -> usize {
        100
    }

    fn max_batch_chars(&self) -> usize {
        25000
    }

    fn translate_batch(
        &self,
        items: &[Item],
        _glossary: &[(String, String)],
        _context: &str,
    ) -> Result<BatchResult, EngineError> {
        let texts: Vec<&str> = items.iter().map(|item| item.text.as_str()).collect();
        let body = json!({"q": texts, "source": "en", "target": self.target, "format": "text"});
        let url = format!("{URL}?key={}", percent_encode(&self.api_key));
        let headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        let (status, text, retry_after) = self.http.post_json(&url, &headers, &body)?;
        check_response(status, &text, retry_after)?;
        let data: Value = serde_json::from_str(&text)
            .map_err(|_| EngineError::Other("Google yanıtı beklenen biçimde değil.".into()))?;
        let translations = data
            .get("data")
            .and_then(|inner| inner.get("translations"))
            .and_then(Value::as_array)
            .ok_or_else(|| EngineError::Other("Google yanıtı beklenen biçimde değil.".into()))?;
        if translations.len() != items.len() {
            return Err(EngineError::Other(
                "Google yanıtındaki çeviri sayısı girdiyle eşleşmiyor.".into(),
            ));
        }
        // Google metni HTML kaçışlarıyla döndürebilir (&#39; gibi); çözülür.
        let result: BTreeMap<usize, String> = items
            .iter()
            .zip(translations)
            .map(|(item, tr)| {
                let raw = tr
                    .get("translatedText")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                (item.key, html_unescape(raw))
            })
            .collect();
        let characters: usize = items.iter().map(|item| item.text.chars().count()).sum();
        Ok(BatchResult {
            translations: result,
            usage: BTreeMap::from([("characters".to_string(), characters as u64)]),
            ..BatchResult::default()
        })
    }
}

fn percent_encode(raw: &str) -> String {
    raw.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(byte).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// Yaygın HTML varlıklarını çözer (adlandırılmış: amp, lt, gt, quot, apos, nbsp; sayısal: &#N; ve &#xH;).
fn html_unescape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        match decode_entity(tail) {
            Some((decoded, len)) => {
                out.push(decoded);
                rest = &tail[len..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_entity(tail: &str) -> Option<(char, usize)> {
    let semi = tail.find(';')?;
    if semi > 10 {
        return None;
    }
    let body = &tail[1..semi];
    let decoded = if let Some(number) = body.strip_prefix('#') {
        let code = match number.strip_prefix(|c| c == 'x' || c == 'X') {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => number.parse::<u32>().ok()?,
        };
        char::from_u32(code).unwrap_or('\u{FFFD}')
    } else {
        match body {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            "nbsp" => '\u{a0}',
            _ => return None,
        }
    };
    Some((decoded, semi + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engines::test_support::{ok_json, FakeHttp};

    fn items() -> Vec<Item> {
        vec![
            Item {
                key: 1,
                id: "x".into(),
                text: "Hello".into(),
            },
            Item {
                key: 2,
                id: "y".into(),
                text: "World".into(),
            },
        ]
    }

    #[test]
    fn request_and_parse_unescapes_html() {
        let http = FakeHttp::with(vec![ok_json(json!({
            "data": {"translations": [{"translatedText": "Oyuncu&#39;nun"}, {"translatedText": "Dünya"}]}
        }))]);
        let engine = GoogleEngine::new(http.clone(), "GKEY", "tr").unwrap();
        let result = engine.translate_batch(&items(), &[], "").unwrap();
        let calls = http.calls.lock().unwrap();
        assert!(calls[0].url.starts_with(URL));
        assert!(calls[0].url.ends_with("?key=GKEY"));
        assert_eq!(
            calls[0].body,
            json!({"q": ["Hello", "World"], "source": "en", "target": "tr", "format": "text"})
        );
        assert_eq!(
            result.translations,
            BTreeMap::from([(1, "Oyuncu'nun".into()), (2, "Dünya".into())])
        );
        assert_eq!(engine.max_items(), 100);
    }

    #[test]
    fn target_code_comes_from_language_table() {
        let http = FakeHttp::with(vec![ok_json(json!({
            "data": {"translations": [{"translatedText": "你好"}, {"translatedText": "世界"}]}
        }))]);
        let engine = GoogleEngine::new(http.clone(), "GKEY", "zh-CN").unwrap();
        engine.translate_batch(&items(), &[], "").unwrap();
        assert_eq!(http.calls.lock().unwrap()[0].body["target"], "zh-CN");
        assert!(GoogleEngine::new(FakeHttp::with(vec![]), "k", "xx").is_err());
    }

    #[test]
    fn unauthorized_is_not_retryable() {
        let engine =
            GoogleEngine::new(FakeHttp::with(vec![(401, "{}".into(), None)]), "k", "tr").unwrap();
        let err = engine.translate_batch(&items(), &[], "").unwrap_err();
        assert!(!err.is_retryable_api());
    }

    #[test]
    fn html_unescape_handles_numeric_and_named() {
        assert_eq!(
            html_unescape("a &amp; b &lt;c&gt; &#x41;&#66;"),
            "a & b <c> AB"
        );
        assert_eq!(html_unescape("&unknown; &"), "&unknown; &");
    }
}
