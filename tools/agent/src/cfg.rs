// Конфигурация: где ящик, где логи сессий и кто я в дереве подагентов.
// Переменные окружения приоритетнее config.toml — харнесс передаёт их тулу, а
// запущенный человеком тул читает файл.
use std::path::PathBuf;

use serde::Deserialize;

pub struct Cfg {
    pub mail_dir: PathBuf,
    pub sessions_dir: PathBuf,
    /// id моей сессии (SNENS_SESSION); пусто, если тул запустил человек руками.
    pub me: String,
    /// id родителя (SNENS_PARENT); пусто у корневой сессии.
    pub parent: String,
}

#[derive(Deserialize, Default)]
struct File {
    #[serde(default)]
    mail_dir: String,
    #[serde(default)]
    sessions_dir: String,
}

/// Домашний каталог. Пустой `HOME` (бывает в systemd-юнитах и контейнерах) раньше
/// схлопывался в `""`, и умолчания превращались в пути от корня файловой
/// системы: `/.local/share/snens/mail`.
fn home_dir() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| ".".to_string())
}

/// Развернуть `~` только в начале пути.
///
/// `str::replace('~', …)` менял **каждый** тильду: `/srv/~backup/mail` становился
/// `/srv//home/user/backup/mail`, а при пустом `HOME` — относительным путём.
fn expand(configured: &str, home: &str, fallback: &str) -> String {
    if configured.is_empty() {
        return fallback.replace("{home}", home);
    }
    match configured.strip_prefix('~') {
        Some("") => home.to_string(),
        Some(rest) if rest.starts_with('/') || rest.starts_with('\\') => {
            format!("{home}{rest}")
        }
        _ => configured.to_string(),
    }
}

impl Cfg {
    pub fn load() -> Cfg {
        let home = home_dir();
        let config_path = std::env::var("SNENS_CONFIG")
            .ok()
            .unwrap_or_else(|| format!("{home}/.config/snens/config.toml"));
        let f: File = match std::fs::read_to_string(&config_path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(file) => file,
                // Молчаливые дефолты уводили ящик в чужой каталог: опечатка в
                // ключе или битый TOML выглядели как «всё настроено по-старому».
                Err(error) => {
                    eprintln!("agent: {config_path} не разобран ({error}) — беру умолчания");
                    File::default()
                }
            },
            Err(_) => File::default(),
        };
        let mail = std::env::var("SNENS_MAIL_DIR")
            .unwrap_or_else(|_| expand(&f.mail_dir, &home, "{home}/.local/share/snens/mail"));
        // SNENS_SESSIONS_DIR — чтобы status/wait можно было проверять на копии логов
        // и смотреть чужие инстансы, не трогая свой config.toml.
        let sessions = std::env::var("SNENS_SESSIONS_DIR").unwrap_or_else(|_| {
            expand(&f.sessions_dir, &home, "{home}/.local/share/snens/sessions")
        });
        Cfg {
            mail_dir: PathBuf::from(mail),
            sessions_dir: PathBuf::from(sessions),
            me: std::env::var("SNENS_SESSION").unwrap_or_default(),
            parent: std::env::var("SNENS_PARENT").unwrap_or_default(),
        }
    }

    pub fn parent_or_me(&self) -> String {
        if self.parent.is_empty() {
            self.me.clone()
        } else {
            self.parent.clone()
        }
    }

    /// Мой ящик или паника: без SNENS_SESSION говорить не о чем.
    pub fn me_or_die(&self, what: &str) -> String {
        if self.me.is_empty() {
            crate::die(format!(
                "agent {what}: не знаю свою сессию (нет SNENS_SESSION)"
            ));
        }
        self.me.clone()
    }
}
