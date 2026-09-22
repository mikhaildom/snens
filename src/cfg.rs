// Конфиг: один TOML, ничего больше. Провайдерские квирки — через body_patch/headers,
// чтобы новый провайдер не требовал кода.
use std::{collections::BTreeMap, path::PathBuf};

use serde::Deserialize;

use crate::R;

#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    #[serde(default = "d_base_url")]
    pub base_url: String,
    #[serde(default = "d_model")]
    pub model: String,
    #[serde(default = "d_key_env")]
    pub key_env: String,
    /// Произвольные поля в тело запроса (quirks конкретного провайдера).
    #[serde(default)]
    pub body_patch: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "d_true")]
    pub stream_usage: bool,
    /// Размер окна модели в токенах (не хардкодим «1M у всех»).
    #[serde(default = "d_ctx")]
    pub ctx_window: u64,
    /// Порог элизии (доля окна).
    #[serde(default = "d_soft")]
    pub soft: f64,
    /// Порог саммаризации (доля окна).
    #[serde(default = "d_hard")]
    pub hard: f64,
    /// Сколько символов одного tool-результата доезжает до модели.
    #[serde(default = "d_max_chars")]
    pub max_result_chars: usize,
    /// Таймаут одного `run` по умолчанию, секунды.
    #[serde(default = "d_timeout")]
    pub timeout_seconds: u64,
    /// Верхняя граница вывода процесса, символы.
    #[serde(default = "d_max_proc")]
    pub max_process_chars: usize,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default = "d_sessions")]
    pub sessions_dir: String,
    /// Каталог тулов: подкаталог = тул. Рядом с ним создаётся bin/ с симлинками.
    #[serde(default = "d_tools")]
    pub tools_dir: String,
}

fn d_base_url() -> String { "https://inference-api.nousresearch.com/v1".into() }
fn d_model() -> String { "deepseek/deepseek-v4.1-flash".into() }
fn d_key_env() -> String { "SNENS_API_KEY".into() }
fn d_true() -> bool { true }
fn d_ctx() -> u64 { 1_000_000 }
fn d_soft() -> f64 { 0.60 }
fn d_hard() -> f64 { 0.85 }
fn d_max_chars() -> usize { 24_000 }
fn d_timeout() -> u64 { 120 }
fn d_max_proc() -> usize { 40_000 }
fn d_sessions() -> String { format!("{}/.local/share/snens/sessions", home()) }
fn d_tools() -> String { format!("{}/.local/share/snens/tools", home()) }

pub fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| ".".into())
}

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("SNENS_CONFIG") {
        return PathBuf::from(p);
    }
    PathBuf::from(format!("{}/.config/snens/config.toml", home()))
}

impl Default for Config {
    fn default() -> Self {
        toml::from_str("").expect("defaults must deserialize")
    }
}

impl Config {
    pub fn load(override_path: Option<&str>) -> R<Config> {
        let path = match override_path {
            Some(p) => PathBuf::from(p),
            None => config_path(),
        };
        let cfg: Config = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| format!("{}: {e}", path.display()))?,
            // Нет файла — работаем на дефолтах, это не ошибка.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(format!("{}: {e}", path.display()).into()),
        };
        // ponytail: путь к сессиям из конфига может содержать ~, разворачиваем один раз здесь
        let mut cfg = cfg;
        cfg.sessions_dir = cfg.sessions_dir.replace('~', &home());
        cfg.tools_dir = cfg.tools_dir.replace('~', &home());
        Ok(cfg)
    }

    pub fn sessions_path(&self) -> PathBuf {
        PathBuf::from(&self.sessions_dir)
    }

    pub fn tools_path(&self) -> PathBuf {
        PathBuf::from(&self.tools_dir)
    }

    /// Каталог симлинков на тулы — сосед каталога тулов, чтобы всё жило в одном месте.
    pub fn bin_path(&self) -> PathBuf {
        self.tools_path()
            .parent()
            .map(|p| p.join("bin"))
            .unwrap_or_else(|| PathBuf::from("bin"))
    }

    pub fn api_key(&self) -> R<String> {
        std::env::var(&self.key_env)
            .map_err(|_| format!("переменная окружения {} не задана", self.key_env).into())
    }
}

pub fn system_prompt(cfg: &Config) -> String {
    match &cfg.system_prompt {
        Some(p) => std::fs::read_to_string(p).unwrap_or_else(|_| default_system()),
        None => default_system(),
    }
}

fn default_system() -> String {
    "Ты агент. Работаешь через инструменты. `run` исполняет shell-команду. \
     Остальные инструменты — программы в PATH: у каждой есть `--help` с описанием и примерами.\n\
     Отвечай кратко. Перед изменением файла сначала прочитай его. \
     Не выдумывай результат команды — сначала выполни."
        .into()
}
