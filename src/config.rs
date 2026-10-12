use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::lang;
use crate::util::atomic_json;

pub const ENGINES: [&str; 4] = ["yandex", "google", "openai", "anthropic"];
const DEFAULT_GAME_PATH: &str = r"D:\Games\steamapps\common\Wuthering Waves";
pub const DEFAULT_PACK_URL: &str =
    "https://raw.githubusercontent.com/Pulsariums/wuwalang/main/packs/{lang}";

pub fn project_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn default_path() -> PathBuf {
    project_dir().join("config.json")
}

pub fn tools_dir() -> PathBuf {
    project_dir().join("tools")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub engine: String,
    pub target: String,
    pub game_path: String,
    pub game_exe: String,
    pub accepted_risk: bool,
    pub pack_url: String,
    pub yandex: YandexConfig,
    pub google: GoogleConfig,
    pub openai: OpenAiConfig,
    pub anthropic: AnthropicConfig,
    pub batch: BatchConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct YandexConfig {
    pub api_key: String,
    pub folder_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GoogleConfig {
    pub api_key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnthropicConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BatchConfig {
    pub max_items: usize,
    pub max_chars: usize,
    pub threads: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            engine: "anthropic".into(),
            target: "tr".into(),
            game_path: DEFAULT_GAME_PATH.into(),
            game_exe: String::new(),
            accepted_risk: false,
            pack_url: DEFAULT_PACK_URL.into(),
            yandex: YandexConfig::default(),
            google: GoogleConfig::default(),
            openai: OpenAiConfig::default(),
            anthropic: AnthropicConfig::default(),
            batch: BatchConfig::default(),
        }
    }
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            model: String::new(),
        }
    }
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.anthropic.com".into(),
            api_key: String::new(),
            model: String::new(),
            max_tokens: 4096,
        }
    }
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            max_items: 150,
            max_chars: 12000,
            threads: 4,
        }
    }
}

fn merge(base: &mut Map<String, Value>, over: &Map<String, Value>) {
    for (key, value) in over {
        if let Some(existing) = base.get_mut(key) {
            if let (Value::Object(inner), Value::Object(incoming)) = (existing, value) {
                merge(inner, incoming);
                continue;
            }
        }
        base.insert(key.clone(), value.clone());
    }
}

/// config.json dosyasını birleştirerek yazar; dosyada olup güncellenmeyen alanlar korunur.
pub fn save(updates: &Value, path: &Path) -> Result<()> {
    let mut data = if path.exists() {
        let text = fs::read_to_string(path)?;
        serde_json::from_str::<Value>(&text)?
    } else {
        Value::Object(Map::new())
    };
    if let (Value::Object(base), Value::Object(over)) = (&mut data, updates) {
        merge(base, over);
    }
    atomic_json(path, &data)
}

/// config.json dosyasını okur, varsayılanlarla birleştirir ve ortam anahtarlarını uygular.
pub fn load(path: &Path, env: &HashMap<String, String>) -> Result<Config> {
    let mut merged = serde_json::to_value(Config::default())?;
    if path.exists() {
        let text = fs::read_to_string(path)?;
        let data: Value = serde_json::from_str(&text)
            .with_context(|| format!("{} geçerli JSON değil", path.display()))?;
        if let (Value::Object(base), Value::Object(over)) = (&mut merged, &data) {
            merge(base, over);
        }
    }
    for engine in ENGINES {
        let Some(key) = env.get(&format!("WWTR_{}_API_KEY", engine.to_uppercase())) else {
            continue;
        };
        if key.is_empty() {
            continue;
        }
        let Some(section) = merged.get_mut(engine).and_then(Value::as_object_mut) else {
            bail!("{engine} bölümü bir nesne olmalı.");
        };
        section.insert("api_key".into(), Value::String(key.clone()));
    }
    if let Some(name) = env.get("WWTR_ENGINE").filter(|name| !name.is_empty()) {
        if let Value::Object(root) = &mut merged {
            root.insert("engine".into(), Value::String(name.clone()));
        }
    }
    let mut cfg: Config = serde_json::from_value(merged).context("config.json okunamadı")?;
    cfg.target = lang::validate(&cfg.target)?.code.to_string();
    if !ENGINES.contains(&cfg.engine.as_str()) {
        bail!(
            "Bilinmeyen motor: {}. Seçenekler: {}",
            cfg.engine,
            ENGINES.join(", ")
        );
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load(&dir.path().join("none.json"), &HashMap::new()).unwrap();
        assert_eq!(cfg.engine, "anthropic");
        assert_eq!(cfg.batch.max_items, 150);
    }

    #[test]
    fn env_api_key_overrides_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, r#"{"engine":"google","google":{"api_key":"file"}}"#).unwrap();
        let env = HashMap::from([("WWTR_GOOGLE_API_KEY".to_string(), "env".to_string())]);
        let cfg = load(&path, &env).unwrap();
        assert_eq!(cfg.engine, "google");
        assert_eq!(cfg.google.api_key, "env");
    }

    #[test]
    fn old_config_without_new_fields_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, r#"{"engine":"google","game_path":"C:\\Oyun"}"#).unwrap();
        let cfg = load(&path, &HashMap::new()).unwrap();
        assert_eq!(cfg.game_path, "C:\\Oyun");
        assert_eq!(cfg.game_exe, "");
        assert!(!cfg.accepted_risk);
    }

    #[test]
    fn pack_url_defaults_when_missing_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, r#"{"engine":"google"}"#).unwrap();
        assert_eq!(
            load(&path, &HashMap::new()).unwrap().pack_url,
            DEFAULT_PACK_URL
        );
        save(
            &serde_json::json!({"pack_url": "https://ornek.test/tr"}),
            &path,
        )
        .unwrap();
        assert_eq!(
            load(&path, &HashMap::new()).unwrap().pack_url,
            "https://ornek.test/tr"
        );
    }

    #[test]
    fn game_exe_and_risk_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        save(
            &serde_json::json!({"game_exe": "C:\\Oyun\\Wuthering Waves.exe", "accepted_risk": true}),
            &path,
        )
        .unwrap();
        let cfg = load(&path, &HashMap::new()).unwrap();
        assert_eq!(cfg.game_exe, "C:\\Oyun\\Wuthering Waves.exe");
        assert!(cfg.accepted_risk);
    }

    #[test]
    fn target_defaults_to_tr_and_is_validated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        assert_eq!(load(&path, &HashMap::new()).unwrap().target, "tr");
        fs::write(&path, r#"{"target":"PT-br"}"#).unwrap();
        assert_eq!(load(&path, &HashMap::new()).unwrap().target, "pt-BR");
        fs::write(&path, r#"{"target":"en"}"#).unwrap();
        assert!(load(&path, &HashMap::new()).is_err());
    }

    #[test]
    fn default_pack_url_has_lang_placeholder() {
        assert!(DEFAULT_PACK_URL.ends_with("/packs/{lang}"));
    }

    #[test]
    fn invalid_engine_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, r#"{"engine":"bilinmeyen"}"#).unwrap();
        assert!(load(&path, &HashMap::new()).is_err());
    }

    #[test]
    fn save_keeps_untouched_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(
            &path,
            r#"{"engine":"openai","extra":1,"openai":{"model":"m1"}}"#,
        )
        .unwrap();
        save(&serde_json::json!({"openai": {"api_key": "k"}}), &path).unwrap();
        let data: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(data["engine"], "openai");
        assert_eq!(data["extra"], 1);
        assert_eq!(data["openai"]["model"], "m1");
        assert_eq!(data["openai"]["api_key"], "k");
    }
}
