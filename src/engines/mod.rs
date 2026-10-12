use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::config::Config;

pub mod anthropic_compat;
pub mod google;
pub mod openai_compat;
pub mod yandex;

pub use anthropic_compat::AnthropicCompatEngine;
pub use google::GoogleEngine;
pub use openai_compat::OpenAiCompatEngine;
pub use yandex::YandexEngine;

/// Bir pakette çevrilecek tek metin. key paket içindeki kısa indekstir (1..n).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub key: usize,
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchResult {
    pub translations: BTreeMap<usize, String>,
    pub new_terms: BTreeMap<String, String>,
    pub usage: BTreeMap<String, u64>,
}

#[derive(Debug)]
pub enum EngineError {
    /// HTTP hatası. 429 ve 5xx yeniden denenebilir kabul edilir.
    Api {
        status: u16,
        message: String,
        retry_after: Option<f64>,
    },
    /// Ağ/bağlantı hatası (yeniden denenir).
    Transport(String),
    /// Beklenmeyen yanıt biçimi veya yapılandırma hatası (yeniden denenir).
    Other(String),
}

impl EngineError {
    pub fn is_retryable_api(&self) -> bool {
        match self {
            Self::Api { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api {
                status, message, ..
            } => {
                let shown: String = message.chars().take(300).collect();
                write!(f, "HTTP {status}: {shown}")
            }
            Self::Transport(message) | Self::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for EngineError {}

/// (durum kodu, gövde, Retry-After saniye)
pub type HttpReply = (u16, String, Option<f64>);

/// HTTP katmanı: gerçek uygulama ureq, testlerde sahte bir istemci kullanılır.
pub trait HttpClient: Send + Sync {
    fn post_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
    ) -> Result<HttpReply, EngineError>;
}

pub struct UreqClient {
    timeout: Duration,
}

impl UreqClient {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl HttpClient for UreqClient {
    fn post_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
    ) -> Result<HttpReply, EngineError> {
        let payload = serde_json::to_string(body).map_err(|e| EngineError::Other(e.to_string()))?;
        let mut request = ureq::post(url).timeout(self.timeout);
        let mut has_content_type = false;
        for (name, value) in headers {
            has_content_type |= name.eq_ignore_ascii_case("content-type");
            request = request.set(name, value);
        }
        if !has_content_type {
            request = request.set("Content-Type", "application/json");
        }
        let response = match request.send_string(&payload) {
            Ok(response) => response,
            Err(ureq::Error::Status(_, response)) => response,
            Err(ureq::Error::Transport(error)) => {
                return Err(EngineError::Transport(error.to_string()))
            }
        };
        let status = response.status();
        let retry_after = response
            .header("Retry-After")
            .and_then(|raw| raw.trim().parse::<f64>().ok());
        let text = response
            .into_string()
            .map_err(|e| EngineError::Transport(e.to_string()))?;
        Ok((status, text, retry_after))
    }
}

/// HTTP yanıtı hata ise ApiError döndürür.
pub fn check_response(
    status: u16,
    body: &str,
    retry_after: Option<f64>,
) -> Result<(), EngineError> {
    if status < 400 {
        return Ok(());
    }
    Err(EngineError::Api {
        status,
        message: body.to_string(),
        retry_after,
    })
}

/// Çeviri motorları için ortak sözleşme.
pub trait Engine: Send + Sync {
    fn name(&self) -> &str;
    /// Makine çevirisi motorları metni yer tutucu jetonlarıyla alır.
    fn machine_translation(&self) -> bool;
    /// Bir istekteki en fazla öğe sayısı (cfg.batch.max_items ile birlikte en küçük alınır).
    fn max_items(&self) -> usize {
        60
    }
    /// Bir istekteki toplam kaynak karakter üst sınırı.
    fn max_batch_chars(&self) -> usize;
    /// items'ı çevirir. glossary: bu partide geçen terimler (EN -> TR).
    /// context: kalıcı sözlüğün tamamı; yapay zekâ motorlarında sabit sistem bilgisine girer.
    fn translate_batch(
        &self,
        items: &[Item],
        glossary: &[(String, String)],
        context: &str,
    ) -> Result<BatchResult, EngineError>;
}

pub fn make_engine(
    cfg: &Config,
    http: Arc<dyn HttpClient>,
) -> Result<Box<dyn Engine>, EngineError> {
    let max_chars = cfg.batch.max_chars;
    match cfg.engine.as_str() {
        "yandex" => Ok(Box::new(YandexEngine::new(
            http,
            &cfg.yandex.api_key,
            &cfg.yandex.folder_id,
            &cfg.target,
        )?)),
        "google" => Ok(Box::new(GoogleEngine::new(http, &cfg.google.api_key, &cfg.target)?)),
        "openai" => Ok(Box::new(OpenAiCompatEngine::new(
            http,
            &cfg.openai.base_url,
            &cfg.openai.api_key,
            &cfg.openai.model,
            max_chars,
            &cfg.target,
        )?)),
        "anthropic" => Ok(Box::new(AnthropicCompatEngine::new(
            http,
            &cfg.anthropic.base_url,
            &cfg.anthropic.api_key,
            &cfg.anthropic.model,
            cfg.anthropic.max_tokens,
            max_chars,
            &cfg.target,
        )?)),
        other => Err(EngineError::Other(format!("Bilinmeyen motor: {other}"))),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;

    pub struct Recorded {
        pub url: String,
        pub headers: Vec<(String, String)>,
        pub body: Value,
    }

    #[derive(Default)]
    pub struct FakeHttp {
        pub replies: Mutex<VecDeque<HttpReply>>,
        pub calls: Mutex<Vec<Recorded>>,
    }

    impl FakeHttp {
        pub fn with(replies: Vec<HttpReply>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.into()),
                calls: Mutex::new(Vec::new()),
            })
        }

        pub fn header(recorded: &Recorded, name: &str) -> String {
            recorded
                .headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        }
    }

    impl HttpClient for FakeHttp {
        fn post_json(
            &self,
            url: &str,
            headers: &[(String, String)],
            body: &Value,
        ) -> Result<HttpReply, EngineError> {
            self.calls.lock().unwrap().push(Recorded {
                url: url.to_string(),
                headers: headers.to_vec(),
                body: body.clone(),
            });
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| EngineError::Transport("sahte yanıt kalmadı".into()))
        }
    }

    pub fn ok_json(value: Value) -> HttpReply {
        (200, value.to_string(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::FakeHttp;
    use super::*;
    use crate::config::AnthropicConfig;

    #[test]
    fn make_engine_from_config() {
        let mut cfg = Config {
            anthropic: AnthropicConfig {
                base_url: "https://a".into(),
                api_key: "k".into(),
                model: "m".into(),
                max_tokens: 10,
            },
            batch: crate::config::BatchConfig {
                max_chars: 1234,
                ..Default::default()
            },
            ..Default::default()
        };
        let engine = make_engine(&cfg, FakeHttp::with(vec![])).unwrap();
        assert_eq!(engine.name(), "anthropic");
        assert_eq!(engine.max_batch_chars(), 1234);
        cfg.engine = "google".into();
        cfg.google.api_key = "g".into();
        assert_eq!(
            make_engine(&cfg, FakeHttp::with(vec![])).unwrap().name(),
            "google"
        );
        cfg.engine = "nope".into();
        assert!(make_engine(&cfg, FakeHttp::with(vec![])).is_err());
    }

    #[test]
    fn unknown_target_is_rejected_by_engines() {
        let cfg = Config {
            engine: "anthropic".into(),
            target: "xx".into(),
            anthropic: AnthropicConfig {
                base_url: "https://a".into(),
                api_key: "k".into(),
                model: "m".into(),
                max_tokens: 10,
            },
            ..Default::default()
        };
        assert!(make_engine(&cfg, FakeHttp::with(vec![])).is_err());
    }
}
