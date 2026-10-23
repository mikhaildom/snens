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

impl Cfg {
    pub fn load() -> Cfg {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_default();
        let f: File = std::env::var("SNENS_CONFIG")
            .ok()
            .or_else(|| Some(format!("{home}/.config/snens/config.toml")))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default();
        let mail = std::env::var("SNENS_MAIL_DIR").unwrap_or_else(|_| {
            if f.mail_dir.is_empty() {
                format!("{home}/.local/share/snens/mail")
            } else {
                f.mail_dir.replace('~', &home)
            }
        });
        // SNENS_SESSIONS_DIR — чтобы status/wait можно было проверять на копии логов
        // и смотреть чужие инстансы, не трогая свой config.toml.
        let sessions = std::env::var("SNENS_SESSIONS_DIR").unwrap_or_else(|_| {
            if f.sessions_dir.is_empty() {
                format!("{home}/.local/share/snens/sessions")
            } else {
                f.sessions_dir.replace('~', &home)
            }
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
