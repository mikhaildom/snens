// Конфиг: один TOML, ничего больше. Провайдерские квирки — через body_patch/headers,
// чтобы новый провайдер не требовал кода.
use std::{collections::BTreeMap, path::Path, path::PathBuf};

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
    /// Сколько последних результатов тулов проекция отдаёт как есть при компакции.
    #[serde(default = "d_keep")]
    pub keep_results: usize,
    /// Сокращать вывод shell-команд (ANSI, длинные строки, повторы) до отправки модели.
    #[serde(default = "d_true")]
    pub reduce_output: bool,
    /// Мультимодальные вложения (канал `SNENS_ATTACH_DIR`). Выключено — ядро не
    /// даёт тулам канал и не отправляет части провайдеру: модель на тексте не
    /// должна получать 400 из-за картинки, которую заведомо не понимает.
    #[serde(default = "d_true")]
    pub attachments: bool,
    /// Потолок одного вложения, байты.
    #[serde(default = "d_max_attach")]
    pub max_attachment_bytes: usize,
    /// Сколько последних вложений проекция отдаёт провайдеру как есть; более
    /// ранние вырезаются при компакции — они дороже текста в разы.
    #[serde(default = "d_keep_attach")]
    pub keep_attachments: usize,
    /// Оболочка для `run`: программа и её флаги. `sh -c` в коде не зашит —
    /// на Windows это `cmd /C`.
    #[serde(default = "d_shell")]
    pub shell: Vec<String>,
    /// Каталог тикетов инстанса: публикуется код, а не история улучшений.
    #[serde(default = "d_tickets")]
    pub tickets_dir: String,
    /// Каталог скиллов: подкаталог = скилл (`SKILL.md` + `references/` + `scripts/`).
    /// У инстанса из исходников указывает в репозиторий: скилл описывает код, значит
    /// версионируется с ним и правится тем же коммитом.
    #[serde(default = "d_skills")]
    pub skills_dir: String,
    /// Сессия длиннее этого числа ходов — повод для ревью (порог по длине, не по цене).
    #[serde(default = "d_review_min")]
    pub review_min_turns: usize,
    /// Потолок ходов на один запуск цикла. Не бюджет по токенам, а защита от
    /// разгона; при достижении цикл не падает, а просит у модели промежуточный итог.
    #[serde(default = "d_max_turns")]
    pub max_turns: u32,
    /// Свой файл промпта ревью вместо вкомпилированного (для опытов).
    #[serde(default)]
    pub review_prompt: Option<String>,
    /// Ящик сессий: канал между подагентом и родителем. Дефолт — рядом с сессиями,
    /// чтобы всё инстанса жило в одном месте.
    #[serde(default = "d_mail")]
    pub mail_dir: String,
    /// Долгая память: программа-провайдер, вызываемая как `<cmd> recall <текст>`
    /// и `<cmd> retain` (JSON хода на stdin). Пусто — канала нет: чистая
    /// установка за модуль не платит, а ядро о нём ничего не знает.
    #[serde(default)]
    pub memory: Memory,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct Memory {
    /// Имя программы в каталоге тулов (или путь, если со слэшем).
    #[serde(default)]
    pub cmd: String,
    /// Таймаут одного вызова, секунды: канал вспомогательный, ход его не ждёт.
    #[serde(default = "d_mem_timeout")]
    pub timeout_seconds: u64,
    /// Теги записи — свой канал в общем банке (у Гермеса свой).
    #[serde(default)]
    pub tag: Vec<String>,
}

fn d_base_url() -> String {
    "https://inference-api.nousresearch.com/v1".into()
}
fn d_model() -> String {
    "deepseek/deepseek-v4.1-flash".into()
}
fn d_key_env() -> String {
    "SNENS_API_KEY".into()
}
fn d_true() -> bool {
    true
}
fn d_ctx() -> u64 {
    1_000_000
}
fn d_soft() -> f64 {
    0.60
}
fn d_hard() -> f64 {
    0.85
}
fn d_max_chars() -> usize {
    24_000
}
fn d_timeout() -> u64 {
    120
}
fn d_max_proc() -> usize {
    40_000
}
fn d_sessions() -> String {
    format!("{}/sessions", data_dir())
}
fn d_tools() -> String {
    format!("{}/tools", data_dir())
}
fn d_keep() -> usize {
    5
}
fn d_max_attach() -> usize {
    32 * 1024 * 1024
}
fn d_keep_attach() -> usize {
    3
}
fn d_mail() -> String {
    format!("{}/mail", data_dir())
}
fn d_tickets() -> String {
    format!("{}/tickets", data_dir())
}
fn d_skills() -> String {
    format!("{}/skills", data_dir())
}
fn d_review_min() -> usize {
    20
}
fn d_max_turns() -> u32 {
    200
}
fn d_mem_timeout() -> u64 {
    60
}
fn d_shell() -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/C".into()]
    } else {
        vec!["sh".into(), "-c".into()]
    }
}

/// Windows: USERPROFILE. unix (Linux, macOS, Termux): HOME.
pub fn home() -> String {
    for k in ["HOME", "USERPROFILE"] {
        if let Ok(v) = std::env::var(k) {
            if !v.is_empty() {
                return v;
            }
        }
    }
    ".".into()
}

/// XDG на unix, APPDATA на Windows. macOS — тоже unix-ветка: `~/.local/share`
/// не маковская конвенция, но своя раскладка на каждую ОС дороже, чем ноль.
pub fn data_dir() -> String {
    if cfg!(windows) {
        if let Ok(a) = std::env::var("APPDATA") {
            return format!("{a}/snens");
        }
    }
    format!("{}/.local/share/snens", home())
}

fn config_dir() -> String {
    if cfg!(windows) {
        if let Ok(a) = std::env::var("APPDATA") {
            return format!("{a}/snens");
        }
    }
    format!("{}/.config/snens", home())
}

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("SNENS_CONFIG") {
        return PathBuf::from(p);
    }
    PathBuf::from(format!("{}/config.toml", config_dir()))
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
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?,
            // Нет файла — работаем на дефолтах, это не ошибка.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(format!("{}: {e}", path.display()).into()),
        };
        // ponytail: путь к сессиям из конфига может содержать ~, разворачиваем один раз здесь
        let mut cfg = cfg;
        cfg.sessions_dir = cfg.sessions_dir.replace('~', &home());
        cfg.tools_dir = cfg.tools_dir.replace('~', &home());
        cfg.tickets_dir = cfg.tickets_dir.replace('~', &home());
        cfg.skills_dir = cfg.skills_dir.replace('~', &home());
        cfg.mail_dir = cfg.mail_dir.replace('~', &home());
        Ok(cfg)
    }

    pub fn sessions_path(&self) -> PathBuf {
        PathBuf::from(&self.sessions_dir)
    }

    /// Каталог данных инстанса: рядом с сессиями лежат `soul.md`, `user.md`, `memory.md`.
    pub fn data_path(&self) -> PathBuf {
        self.sessions_path()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn skills_path(&self) -> PathBuf {
        PathBuf::from(&self.skills_dir)
    }

    /// Репозиторий исходников харнесса: improve правит код здесь. Инсталляция из
    /// исходников — это каталог над tools_dir; иначе конфигурация неполная.
    pub fn repo_path(&self) -> R<PathBuf> {
        let repo = PathBuf::from(&self.tools_dir)
            .parent()
            .map(Path::to_path_buf)
            .ok_or("не могу определить репозиторий из tools_dir")?;
        if !repo.join(".git").exists() {
            return Err(format!(
                "в {} нет .git — улучшению нужен репозиторий исходников",
                repo.display()
            )
            .into());
        }
        Ok(repo)
    }

    pub fn tickets_path(&self) -> PathBuf {
        PathBuf::from(&self.tickets_dir)
    }

    pub fn mail_path(&self) -> PathBuf {
        PathBuf::from(&self.mail_dir)
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

    /// Программа долгой памяти: имя — из каталога тулов (там же, где `bin/`),
    /// путь со слэшем — как задан. Пусто — канала нет.
    pub fn memory_bin(&self) -> Option<PathBuf> {
        let cmd = self.memory.cmd.trim();
        if cmd.is_empty() {
            return None;
        }
        let named = Path::new(cmd);
        let exe = self
            .bin_path()
            .join(format!("{cmd}{}", std::env::consts::EXE_SUFFIX));
        if named.components().count() == 1 && exe.is_file() {
            Some(exe)
        } else {
            Some(PathBuf::from(cmd))
        }
    }

    /// Ключ: сначала переменная окружения, потом `.env` рядом с конфигом —
    /// чтобы snens мог стоять сам, без оболочки с экспортами.
    pub fn api_key(&self) -> R<String> {
        if let Ok(k) = std::env::var(&self.key_env) {
            if !k.trim().is_empty() {
                return Ok(k);
            }
        }
        let path = config_path().with_file_name(".env");
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    if k.trim() == self.key_env {
                        let v = v.trim().trim_matches('"').trim_matches('\'');
                        if !v.is_empty() {
                            return Ok(v.to_string());
                        }
                    }
                }
            }
        }
        Err(format!(
            "ключ не найден: ни ${}, ни {} в {}",
            self.key_env,
            self.key_env,
            path.display()
        )
        .into())
    }
}
