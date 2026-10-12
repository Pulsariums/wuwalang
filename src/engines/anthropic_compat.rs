use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{json, Value};

use super::{check_response, BatchResult, Engine, EngineError, HttpClient, Item};
use crate::lang::{self, Lang};
use crate::prompt::{build_system, build_user, parse_response};

pub const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicCompatEngine {
    http: Arc<dyn HttpClient>,
    url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
    max_chars: usize,
    lang: &'static Lang,
}

impl AnthropicCompatEngine {
    pub fn new(
        http: Arc<dyn HttpClient>,
        base_url: &str,
        api_key: &str,
        model: &str,
        max_tokens: u32,
        max_chars: usize,
        target: &str,
    ) -> Result<Self, EngineError> {
        if model.is_empty() {
            return Err(EngineError::Other(
                "Anthropic motoru için model gerekli.".into(),
            ));
        }
        if api_key.is_empty() {
            return Err(EngineError::Other(
                "Anthropic motoru için api_key gerekli.".into(),
            ));
        }
        Ok(Self {
            http,
            url: format!("{}/v1/messages", base_url.trim_end_matches('/')),
            api_key: api_key.to_string(),
            model: model.to_string(),
            max_tokens,
            max_chars,
            lang: lang::validate(target).map_err(|error| EngineError::Other(error.to_string()))?,
        })
    }
}

impl Engine for AnthropicCompatEngine {
    fn name(&self) -> &str {
        "anthropic"
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
        let headers = vec![
            ("x-api-key".to_string(), self.api_key.clone()),
            (
                "anthropic-version".to_string(),
                ANTHROPIC_VERSION.to_string(),
            ),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        // Sabit kısım (kurallar + kalıcı sözlük) önbelleğe alınır.
        let body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": [{
                "type": "text",
                "text": build_system(self.lang, context),
                "cache_control": {"type": "ephemeral"},
            }],
            "messages": [{"role": "user", "content": build_user(items, glossary)}],
        });
        let (status, text, retry_after) = self.http.post_json(&self.url, &headers, &body)?;
        check_response(status, &text, retry_after)?;
        let data: Value = serde_json::from_str(&text)
            .map_err(|_| EngineError::Other("Anthropic yanıtı beklenen biçimde değil.".into()))?;
        let blocks = data
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| EngineError::Other("Anthropic yanıtı beklenen biçimde değil.".into()))?;
        let reply: String = blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect();
        let mut result = parse_response(&reply, items)?;
        let usage = data.get("usage");
        let read = |key: &str| {
            usage
                .and_then(|u| u.get(key))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        result.usage = BTreeMap::from([
            ("input_tokens".to_string(), read("input_tokens")),
            ("output_tokens".to_string(), read("output_tokens")),
            (
                "cache_read_tokens".to_string(),
                read("cache_read_input_tokens"),
            ),
            (
                "cache_write_tokens".to_string(),
                read("cache_creation_input_tokens"),
            ),
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
    fn request_cache_control_and_parse() {
        let http = FakeHttp::with(vec![ok_json(json!({
            "content": [{"type": "text", "text": r#"{"t":{"1":"Merhaba","2":"Dünya"},"new_terms":{"Rover":"Gezgin"}}"#}],
            "usage": {"input_tokens": 7, "output_tokens": 3, "cache_read_input_tokens": 100}
        }))]);
        let engine = AnthropicCompatEngine::new(
            http.clone(),
            "https://api.anthropic.com",
            "AKEY",
            "claude-x",
            999,
            6000,
            "tr",
        )
        .unwrap();
        let glossary = vec![("Rover".to_string(), "Gezgin".to_string())];
        let result = engine
            .translate_batch(&items(), &glossary, "Rover=Gezgin")
            .unwrap();
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls[0].url, "https://api.anthropic.com/v1/messages");
        assert_eq!(FakeHttp::header(&calls[0], "x-api-key"), "AKEY");
        assert_eq!(
            FakeHttp::header(&calls[0], "anthropic-version"),
            "2023-06-01"
        );
        let body = &calls[0].body;
        assert_eq!(body["max_tokens"], 999);
        assert_eq!(body["model"], "claude-x");
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert!(body["system"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Rover=Gezgin"));
        assert_eq!(body["messages"][0]["role"], "user");
        assert!(body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Rover=Gezgin"));
        assert_eq!(
            result.new_terms,
            BTreeMap::from([("Rover".to_string(), "Gezgin".to_string())])
        );
        assert_eq!(result.usage["cache_read_tokens"], 100);
    }

    #[test]
    fn requires_model_and_key() {
        assert!(
            AnthropicCompatEngine::new(FakeHttp::with(vec![]), "https://a", "k", "", 1, 1, "tr").is_err()
        );
        assert!(
            AnthropicCompatEngine::new(FakeHttp::with(vec![]), "https://a", "", "m", 1, 1, "tr").is_err()
        );
    }
}
