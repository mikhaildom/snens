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
    /// Откуда конфиг прочитан. Нужен, чтобы перечитать `model` в рантайме:
    /// `snens model <slug>` правит файл, а долгоживущие процессы (сервер API)
    /// держат снимок и без этого не видят смены.
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
    /// Модель задана кодом, а не файлом (подагент на `subagent_model`):
    /// перечитывать её из конфига нельзя — файл про основную модель.
    #[serde(skip)]
    pub model_pinned: bool,
    /// Optional override for detached child agents, never the primary session.
    #[serde(default)]
    pub subagent_model: Option<String>,
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
    /// Экономический порог уборки в токенах; 0 — авто по размеру окна.
    #[serde(default)]
    pub context_budget: u64,
    /// Запас до физического предела окна; для маленьких окон не больше четверти.
    #[serde(default = "d_context_reserve")]
    pub context_reserve: u64,
    /// Цель после уборки в токенах; 0 — 70% эффективного бюджета.
    #[serde(default)]
    pub context_target: u64,
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
fn d_context_reserve() -> u64 {
    8_192
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
fn d_max_attach() -> usize {
    32 * 1024 * 1024
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
    /// Экономический бюджет, ограниченный окном с запасом для следующего хода.
    pub fn context_budget(&self) -> u64 {
        let budget = if self.context_budget > 0 {
            self.context_budget
        } else if self.ctx_window <= 300_000 {
            100_000
        } else {
            200_000
        };
        // Дефолтный reserve не должен обнулять синтетические маленькие окна.
        let reserve = self.context_reserve.min(self.ctx_window / 4);
        let physical = self.ctx_window.saturating_sub(reserve);
        let ceiling = (u128::from(physical) * 85 / 100) as u64;
        budget.min(ceiling)
    }

    /// Цель уборки не превышает эффективный экономический бюджет.
    pub fn context_target(&self) -> u64 {
        let budget = self.context_budget();
        let target = if self.context_target > 0 {
            self.context_target
        } else {
            // u128 сохраняет точные 70% без переполнения даже при u64::MAX.
            (u128::from(budget) * 7 / 10) as u64
        };
        target.min(budget)
    }

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
        cfg.source_path = Some(path);
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

    /// Сменить основную модель: правит строку `model = "..."` в конфиге.
    /// Текстом, а не пере-сериализацией TOML: комментарии и порядок полей
    /// человека — часть конфига, `toml::to_string` их сносит. Ключа в файле
    /// нет — вставляем до первой `[секции]`, чтобы остался top-level.
    /// Возвращает путь конфига (что именно поправили).
    pub fn set_model(&self, slug: &str, override_path: Option<&str>) -> R<PathBuf> {
        let slug = slug.trim();
        if slug.is_empty() {
            return Err("пустой slug модели".into());
        }
        let path = match override_path {
            Some(p) => PathBuf::from(p),
            None => config_path(),
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("{}: {e}", path.display()).into()),
        };
        let line = format!("model = \"{slug}\"");
        let mut out = String::new();
        let mut replaced = false;
        let mut inserted = false;
        for raw in text.lines() {
            let key = raw.trim_start();
            let is_model = key
                .strip_prefix("model")
                .map(|rest| rest.trim_start().starts_with('='))
                .unwrap_or(false);
            if !replaced && is_model {
                out.push_str(&line);
                out.push('\n');
                replaced = true;
                continue;
            }
            // Нет ключа: вставляем перед первой секцией (иначе ключ уедет в неё).
            if !replaced && !inserted && key.starts_with('[') {
                out.push_str(&line);
                out.push('\n');
                inserted = true;
            }
            out.push_str(raw);
            out.push('\n');
        }
        if !replaced && !inserted {
            if !out.is_empty() && !out.ends_with("\n\n") {
                out.push('\n');
            }
            out.push_str(&line);
            out.push('\n');
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, out)?;
        // Проверяем, что конфиг остался валидным и модель действительно та.
        let check = Config::load(Some(&path.display().to_string()))?;
        if check.model != slug {
            return Err(format!("модель не применилась: в конфиге {}", check.model).into());
        }
        Ok(path)
    }

    /// Модель для нового хода: перечитать из файла.
    /// Долгоживущий процесс (сервер API) держит снимок конфига, а `snens model
    /// <slug>` правит только файл — без перечитки новый ход уходит старой моделью.
    /// Файла нет или он сломан — остаёмся на снимке: ход не должен падать из-за
    /// правки конфига под руками.
    pub fn model_now(&self) -> String {
        if self.model_pinned {
            return self.model.clone();
        }
        let Some(path) = &self.source_path else {
            return self.model.clone();
        };
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(fresh) => fresh.model,
                Err(_) => self.model.clone(),
            },
            Err(_) => self.model.clone(),
        }
    }

    /// Сменить модель в файле и сразу применить её в этом процессе: без второго
    /// шага сервер продолжал бы ходить старой моделью до перезапуска.
    pub fn set_model_now(&mut self, slug: &str) -> R<PathBuf> {
        let path = match self.source_path.clone() {
            Some(path) => self.set_model(slug, Some(&path.display().to_string()))?,
            None => self.set_model(slug, None)?,
        };
        self.model = slug.trim().to_string();
        self.model_pinned = false;
        Ok(path)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_defaults_and_auto_windows() {
        let cfg = Config::default();
        assert_eq!(cfg.context_budget, 0);
        assert_eq!(cfg.context_reserve, 8_192);
        assert_eq!(cfg.context_target, 0);
        for (window, budget, target) in [
            (250_000, 100_000, 70_000),
            (300_000, 100_000, 70_000),
            (300_001, 200_000, 140_000),
            (1_000_000, 200_000, 140_000),
        ] {
            let cfg: Config = toml::from_str(&format!("ctx_window = {window}")).unwrap();
            assert_eq!(cfg.context_budget(), budget);
            assert_eq!(cfg.context_target(), target);
        }
    }

    #[test]
    fn context_overrides_and_reserve() {
        let mut cfg: Config = toml::from_str(
            "ctx_window = 250000\ncontext_budget = 150000\ncontext_target = 90000\ncontext_reserve = 20000",
        )
        .unwrap();
        assert_eq!(cfg.context_budget(), 150_000);
        assert_eq!(cfg.context_target(), 90_000);
        cfg.context_budget = 250_000;
        cfg.context_target = 240_000;
        assert_eq!(cfg.context_budget(), 195_500);
        assert_eq!(cfg.context_target(), 195_500);
        cfg.context_reserve = 0;
        assert_eq!(cfg.context_budget(), 212_500);
        cfg.context_target = 0;
        assert_eq!(cfg.context_target(), 148_750);
    }

    #[test]
    fn context_tiny_windows_and_extreme_values_are_safe() {
        for (window, budget) in [(0, 0), (1, 0), (3, 2), (4, 2), (400, 255)] {
            let cfg = Config {
                ctx_window: window,
                ..Config::default()
            };
            assert_eq!(cfg.context_budget(), budget);
            assert_eq!(cfg.context_target(), budget * 7 / 10);
        }
        let mut cfg = Config {
            ctx_window: 400,
            context_reserve: u64::MAX,
            ..Config::default()
        };
        assert_eq!(cfg.context_budget(), 255);
        cfg.ctx_window = u64::MAX;
        cfg.context_budget = u64::MAX;
        cfg.context_reserve = 0;
        assert_eq!(
            cfg.context_budget(),
            (u128::from(u64::MAX) * 85 / 100) as u64
        );
        assert_eq!(
            cfg.context_target(),
            (u128::from(cfg.context_budget()) * 7 / 10) as u64
        );
    }

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("snens-cfg-{}-{name}.toml", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn set_model_заменяет_строку_и_хранит_комментарии() {
        let p = tmp("replace");
        std::fs::write(
            &p,
            "# шапка\nmodel = \"старая\"\nbase_url = \"http://x\"\n\n[memory]\ncmd = \"memory\"\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&p.display().to_string())).unwrap();
        cfg.set_model("новая/модель", Some(&p.display().to_string()))
            .unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("# шапка"), "комментарий потерян: {text}");
        assert!(text.contains("model = \"новая/модель\""));
        assert!(!text.contains("старая"));
        // секция осталась секцией, ключ не уехал в неё
        let reload = Config::load(Some(&p.display().to_string())).unwrap();
        assert_eq!(reload.model, "новая/модель");
        assert_eq!(reload.memory.cmd, "memory");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn set_model_вставляет_ключ_перед_секцией() {
        let p = tmp("insert");
        std::fs::write(
            &p,
            "base_url = \"http://x\"\n\n[memory]\ncmd = \"memory\"\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&p.display().to_string())).unwrap();
        cfg.set_model("m1", Some(&p.display().to_string())).unwrap();
        let reload = Config::load(Some(&p.display().to_string())).unwrap();
        assert_eq!(reload.model, "m1");
        assert_eq!(reload.memory.cmd, "memory");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn set_model_пустой_слаг_ошибка() {
        let p = tmp("empty");
        std::fs::write(&p, "model = \"x\"\n").unwrap();
        let cfg = Config::load(Some(&p.display().to_string())).unwrap();
        assert!(cfg.set_model("  ", Some(&p.display().to_string())).is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn model_now_читает_файл_а_не_снимок() {
        let p = tmp("fresh");
        std::fs::write(&p, "model = \"старая\"\n").unwrap();
        let cfg = Config::load(Some(&p.display().to_string())).unwrap();
        assert_eq!(cfg.model_now(), "старая");
        // Правка файла мимо процесса должна быть видна: именно так `snens model`
        // меняет модель у уже запущенного сервера.
        std::fs::write(&p, "model = \"новая\"\n").unwrap();
        assert_eq!(cfg.model_now(), "новая");
        // Сломанный файл не должен ронять ход: остаёмся на снимке.
        std::fs::write(&p, "model = [не toml").unwrap();
        assert_eq!(cfg.model_now(), "старая");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn model_pinned_игнорирует_файл() {
        let p = tmp("pinned");
        std::fs::write(&p, "model = \"основная\"\n").unwrap();
        let mut cfg = Config::load(Some(&p.display().to_string())).unwrap();
        cfg.model = "подагентская".into();
        cfg.model_pinned = true;
        std::fs::write(&p, "model = \"другая\"\n").unwrap();
        assert_eq!(cfg.model_now(), "подагентская");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn set_model_now_меняет_и_файл_и_снимок() {
        let p = tmp("apply");
        std::fs::write(&p, "model = \"старая\"\n").unwrap();
        let mut cfg = Config::load(Some(&p.display().to_string())).unwrap();
        cfg.set_model_now("новая/модель").unwrap();
        assert_eq!(cfg.model, "новая/модель");
        assert_eq!(cfg.model_now(), "новая/модель");
        let _ = std::fs::remove_file(&p);
    }
}
