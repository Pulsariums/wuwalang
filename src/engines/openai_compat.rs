use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{json, Value};

use super::{check_response, BatchResult, Engine, EngineError, HttpClient, Item};
use crate::lang::{self, Lang};
use crate::prompt::{build_system, build_user, parse_response};

pub struct OpenAiCompatEngine {
    http: Arc<dyn HttpClient>,
    url: String,
    api_key: String,
    model: String,
    max_chars: usize,
    lang: &'static Lang,
}

impl OpenAiCompatEngine {
    pub fn new(
        http: Arc<dyn HttpClient>,
        base_url: &str,
        api_key: &str,
        model: &str,
        max_chars: usize,
        target: &str,
    ) -> Result<Self, EngineError> {
        if base_url.is_empty() || model.is_empty() {
            return Err(EngineError::Other(
                "OpenAI uyumlu motor için base_url ve model gerekli.".into(),
            ));
        }
        if api_key.is_empty() {
            return Err(EngineError::Other(
                "OpenAI uyumlu motor için api_key gerekli.".into(),
            ));
        }
        Ok(Self {
            http,
            url: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            api_key: api_key.to_string(),
            model: model.to_string(),
            max_chars,
            lang: lang::validate(target).map_err(|error| EngineError::Other(error.to_string()))?,
        })
    }

    fn post(&self, body: &Value) -> Result<(u16, String, Option<f64>), EngineError> {
        let headers = vec![
            (
                "Authorization".to_string(),
                format!("Bearer {}", self.api_key),
            ),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        self.http.post_json(&self.url, &headers, body)
    }
}

impl Engine for OpenAiCompatEngine {
    fn name(&self) -> &str {
        "openai"
    }

    fn machine_translation(&self) -> bool {
        false
    }

    fn max_items(&self) -> usize {
        1000
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
        let mut body = json!({
            "model": self.model,
            "temperature": 0.2,
            "messages": [
                {"role": "system", "content": build_system(self.lang, context)},
                {"role": "user", "content": build_user(items, glossary)},
            ],
            "response_format": {"type": "json_object"},
        });
        let (mut status, mut text, mut retry_after) = self.post(&body)?;
        if status == 400 && text.contains("response_format") {
            // Bazı sunucular response_format desteklemez; istek bunsuz tekrarlanır, JSON metinden ayıklanır.
            if let Value::Object(map) = &mut body {
                map.remove("response_format");
            }
            (status, text, retry_after) = self.post(&body)?;
        }
        check_response(status, &text, retry_after)?;
        let data: Value = serde_json::from_str(&text).map_err(|_| {
            EngineError::Other("OpenAI uyumlu yanıt beklenen biçimde değil.".into())
        })?;
        if data
            .get("choices")
            .and_then(|choices| choices.get(0))
            .and_then(|choice| choice.get("finish_reason"))
            .and_then(Value::as_str)
            == Some("length")
        {
            return Err(EngineError::Other(
                "Yanıt token sınırında kesildi; batch.max_chars değerini düşürün.".into(),
            ));
        }
        let content = data
            .get("choices")
            .and_then(|choices| choices.get(0))
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .map(|value| value.as_str().unwrap_or(""))
            .ok_or_else(|| {
                EngineError::Other("OpenAI uyumlu yanıt beklenen biçimde değil.".into())
            })?;
        let mut result = parse_response(content, items)?;
        let usage = data.get("usage");
        let read = |key: &str| {
            usage
                .and_then(|u| u.get(key))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        result.usage = BTreeMap::from([
            ("input_tokens".to_string(), read("prompt_tokens")),
            ("output_tokens".to_string(), read("completion_tokens")),
        ]);
        Ok(result)
    }
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
    fn request_and_tolerant_parse() {
        let http = FakeHttp::with(vec![ok_json(json!({
            "choices": [{"message": {"content": r#"{"t":{"1":"Merhaba","2":"Dünya"},"new_terms":{}}"#}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        }))]);
        let engine = OpenAiCompatEngine::new(
            http.clone(),
            "https://api.example.com/v1/",
            "OKEY",
            "m1",
            6000,
            "tr",
        )
        .unwrap();
        let glossary = vec![("Hello".to_string(), "Merhaba".to_string())];
        let result = engine
            .translate_batch(&items(), &glossary, "Hello=Merhaba")
            .unwrap();
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls[0].url, "https://api.example.com/v1/chat/completions");
        assert_eq!(FakeHttp::header(&calls[0], "Authorization"), "Bearer OKEY");
        let body = &calls[0].body;
        assert_eq!(body["model"], "m1");
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["response_format"], json!({"type": "json_object"}));
        assert_eq!(body["messages"][0]["role"], "system");
        assert!(body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Hello=Merhaba"));
        assert_eq!(
            result.translations,
            BTreeMap::from([(1, "Merhaba".into()), (2, "Dünya".into())])
        );
        assert_eq!(result.usage["input_tokens"], 10);
    }

    #[test]
    fn retries_without_response_format() {
        let http = FakeHttp::with(vec![
            (400, "unsupported response_format".into(), None),
            ok_json(
                json!({"choices": [{"message": {"content": r#"Sonuç: {"t":{"1":"A","2":"B"}}"#}}]}),
            ),
        ]);
        let engine = OpenAiCompatEngine::new(http.clone(), "https://h/v1", "k", "m", 6000, "tr").unwrap();
        let result = engine.translate_batch(&items(), &[], "").unwrap();
        let calls = http.calls.lock().unwrap();
        assert!(calls[1].body.get("response_format").is_none());
        assert_eq!(
            result.translations,
            BTreeMap::from([(1, "A".into()), (2, "B".into())])
        );
    }

    #[test]
    fn requires_model_and_key() {
        assert!(OpenAiCompatEngine::new(FakeHttp::with(vec![]), "https://h", "k", "", 1, "tr").is_err());
        assert!(OpenAiCompatEngine::new(FakeHttp::with(vec![]), "https://h", "", "m", 1, "tr").is_err());
    }
}
