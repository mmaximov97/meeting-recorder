//! Настройки, переживающие перезапуск.
//!
//! Живут в GUI, а не в ядре, намеренно: `App` тестируется без файловой системы,
//! и чтение конфига внутри него отняло бы эту способность. Ядро принимает выбор
//! устройства как значение — где оно хранится, его не касается.

use meeting_recorder::capture::DeviceChoice;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Eq, Debug)]
#[serde(default)]
pub struct Config {
    /// Идентификатор эндпоинта (`DeviceId` строкой). `None` — системный дефолт.
    pub mic_device_id: Option<String>,
    /// Имя на момент выбора. Только для показа: подставляется в выпадашку и в
    /// предупреждение о подмене, чтобы пользователь видел «Headset (Boss Bose)»,
    /// а не `{0.0.1.00000000}.{guid}`. Матчинг по нему не идёт нигде.
    pub mic_device_name: Option<String>,
    /// Базовый URL шлюза, например `http://your-gateway.local:8080` — без хвоста
    /// `/v1/...`, его дописывает клиент транскрипции.
    pub stt_gateway_url: Option<String>,
    pub stt_api_key: Option<String>,
}

impl Config {
    pub fn choice(&self) -> DeviceChoice {
        match &self.mic_device_id {
            Some(id) => DeviceChoice::Id(id.clone()),
            None => DeviceChoice::Default,
        }
    }

    /// Разбор без единого способа упасть: битый или чужой JSON даёт дефолт.
    /// Потерять настройку неприятно, не записать встречу — хуже.
    pub fn from_str(s: &str) -> Self {
        serde_json::from_str(s).unwrap_or_default()
    }

    fn path(app: &AppHandle) -> Result<PathBuf, String> {
        let dir = app
            .path()
            .app_config_dir()
            .map_err(|e| format!("не найден каталог конфига: {e}"))?;
        Ok(dir.join("config.json"))
    }

    pub fn load(app: &AppHandle) -> Self {
        match Self::path(app).and_then(|p| std::fs::read_to_string(p).map_err(|e| e.to_string())) {
            Ok(s) => Self::from_str(&s),
            // Файла нет при первом запуске — это не ошибка.
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, app: &AppHandle) -> Result<(), String> {
        let path = Self::path(app)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("не удалось создать {}: {e}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, json).map_err(|e| format!("не удалось записать {}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn пустой_конфиг_это_системный_дефолт() {
        assert_eq!(Config::default().choice(), DeviceChoice::Default);
    }

    #[test]
    fn идентификатор_устройства_превращается_в_id() {
        let c = Config {
            mic_device_id: Some("{0.0.1.00000000}.{guid}".into()),
            mic_device_name: Some("Headset (Boss Bose)".into()),
            stt_gateway_url: None,
            stt_api_key: None,
        };
        assert_eq!(c.choice(), DeviceChoice::Id("{0.0.1.00000000}.{guid}".into()));
    }

    /// Имя — только для показа. Конфиг с одним именем и без идентификатора
    /// матчить нечем, и притворяться, что устройство выбрано, нельзя.
    #[test]
    fn одно_имя_без_идентификатора_это_дефолт() {
        let c = Config {
            mic_device_id: None,
            mic_device_name: Some("Headset (Boss Bose)".into()),
            stt_gateway_url: None,
            stt_api_key: None,
        };
        assert_eq!(c.choice(), DeviceChoice::Default);
    }

    /// Битый JSON — это потерянная настройка, а не потерянная запись.
    #[test]
    fn битый_json_даёт_дефолт_а_не_панику() {
        assert_eq!(Config::from_str("{ это не json"), Config::default());
    }

    #[test]
    fn незнакомые_поля_не_ломают_разбор() {
        let c = Config::from_str(r#"{"mic_device_id":"{id}","что_то_новое":42}"#);
        assert_eq!(c.mic_device_id.as_deref(), Some("{id}"));
        assert_eq!(c.mic_device_name, None, "отсутствующее поле — не ошибка");
    }

    #[test]
    fn настройки_транскрипции_переживают_сериализацию() {
        let c = Config {
            mic_device_id: None,
            mic_device_name: None,
            stt_gateway_url: Some("http://your-gateway.local:8080".to_string()),
            stt_api_key: Some("ailab_xxx".to_string()),
        };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(Config::from_str(&json), c);
    }

    #[test]
    fn старый_конфиг_без_настроек_транскрипции_даёт_none() {
        let c = Config::from_str(r#"{"mic_device_id":"{id}"}"#);
        assert_eq!(c.stt_gateway_url, None);
        assert_eq!(c.stt_api_key, None);
    }

    #[test]
    fn смена_только_микрофонных_полей_не_трогает_остальные() {
        let mut cfg = Config {
            mic_device_id: None,
            mic_device_name: None,
            stt_gateway_url: Some("http://your-gateway.local:8080".to_string()),
            stt_api_key: Some("secret".to_string()),
        };
        cfg.mic_device_id = Some("{new-id}".to_string());
        cfg.mic_device_name = Some("Новый микрофон".to_string());
        assert_eq!(cfg.stt_gateway_url.as_deref(), Some("http://your-gateway.local:8080"));
        assert_eq!(cfg.stt_api_key.as_deref(), Some("secret"));
    }
}
