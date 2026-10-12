use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{json, Value};

use super::{check_response, BatchResult, Engine, EngineError, HttpClient, Item};
use crate::lang;

pub const URL: &str = "https://translate.api.cloud.yandex.net/translate/v2/translate";

pub struct YandexEngine {
    http: Arc<dyn HttpClient>,
    api_key: String,
    folder_id: String,
    target: &'static str,
}

impl YandexEngine {
    pub fn new(
        http: Arc<dyn HttpClient>,
        api_key: &str,
        folder_id: &str,
        target: &str,
    ) -> Result<Self, EngineError> {
        if api_key.is_empty() || folder_id.is_empty() {
            return Err(EngineError::Other(
                "Yandex için api_key ve folder_id gerekli.".into(),
            ));
        }
        Ok(Self {
            http,
            api_key: api_key.to_string(),
            folder_id: folder_id.to_string(),
            target: lang::validate(target)
                .map_err(|error| EngineError::Other(error.to_string()))?
                .yandex,
        })
    }
}

impl Engine for YandexEngine {
    fn name(&self) -> &str {
        "yandex"
    }

    fn machine_translation(&self) -> bool {
        true
    }

    fn max_batch_chars(&self) -> usize {
        // İstek başına toplam ~10000 karakter sınırı; güvenlik payı bırakılır.
        9000
    }

    fn translate_batch(
        &self,
        items: &[Item],
        _glossary: &[(String, String)],
        _context: &str,
    ) -> Result<BatchResult, EngineError> {
        let texts: Vec<&str> = items.iter().map(|item| item.text.as_str()).collect();
        let body = json!({
            "folderId": self.folder_id,
            "texts": texts,
            "sourceLanguageCode": "en",
            "targetLanguageCode": self.target,
            "format": "PLAIN_TEXT",
        });
        let headers = vec![
            (
                "Authorization".to_string(),
                format!("Api-Key {}", self.api_key),
            ),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        let (status, text, retry_after) = self.http.post_json(URL, &headers, &body)?;
        check_response(status, &text, retry_after)?;
        let data: Value = serde_json::from_str(&text)
            .map_err(|_| EngineError::Other("Yandex yanıtı beklenen biçimde değil.".into()))?;
        let translations = data
            .get("translations")
            .and_then(Value::as_array)
            .ok_or_else(|| EngineError::Other("Yandex yanıtı beklenen biçimde değil.".into()))?;
        if translations.len() != items.len() {
            return Err(EngineError::Other(
                "Yandex yanıtındaki çeviri sayısı girdiyle eşleşmiyor.".into(),
            ));
        }
        let result: BTreeMap<usize, String> = items
            .iter()
            .zip(translations)
            .map(|(item, tr)| {
                let text = tr.get("text").and_then(Value::as_str).unwrap_or("");
                (item.key, text.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engines::test_support::{ok_json, FakeHttp};
    use crate::engines::HttpReply;

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
    fn request_and_parse() {
        let http = FakeHttp::with(vec![ok_json(json!({
            "translations": [{"text": "Merhaba"}, {"text": "Dünya"}]
        }))]);
        let engine = YandexEngine::new(http.clone(), "KEY", "FOLDER", "tr").unwrap();
        let result = engine.translate_batch(&items(), &[], "").unwrap();
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls[0].url, URL);
        assert_eq!(FakeHttp::header(&calls[0], "Authorization"), "Api-Key KEY");
        assert_eq!(
            calls[0].body,
            json!({
                "folderId": "FOLDER",
                "texts": ["Hello", "World"],
                "sourceLanguageCode": "en",
                "targetLanguageCode": "tr",
                "format": "PLAIN_TEXT",
            })
        );
        assert_eq!(
            result.translations,
            BTreeMap::from([(1, "Merhaba".into()), (2, "Dünya".into())])
        );
    }

    #[test]
    fn target_code_comes_from_language_table() {
        let http = FakeHttp::with(vec![ok_json(json!({
            "translations": [{"text": "Hallo"}, {"text": "Welt"}]
        }))]);
        let engine = YandexEngine::new(http.clone(), "KEY", "FOLDER", "de").unwrap();
        engine.translate_batch(&items(), &[], "").unwrap();
        assert_eq!(http.calls.lock().unwrap()[0].body["targetLanguageCode"], "de");
        assert!(YandexEngine::new(FakeHttp::with(vec![]), "k", "f", "xx").is_err());
    }

    #[test]
    fn requires_credentials() {
        assert!(YandexEngine::new(FakeHttp::with(vec![]), "", "FOLDER", "tr").is_err());
    }

    #[test]
    fn http_errors_become_api_errors() {
        let replies: Vec<HttpReply> = vec![(429, "{}".into(), Some(7.0))];
        let engine = YandexEngine::new(FakeHttp::with(replies), "k", "f", "tr").unwrap();
        match engine.translate_batch(&items(), &[], "") {
            Err(err @ EngineError::Api { .. }) => {
                assert!(err.is_retryable_api());
                assert!(matches!(err, EngineError::Api { retry_after: Some(r), .. } if r == 7.0));
            }
            other => panic!("beklenmeyen sonuç: {other:?}"),
        }
    }
}
