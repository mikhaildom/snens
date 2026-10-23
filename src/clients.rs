// Реестр подключённых устройств: у каждого свой токен, а не один общий.
//
// Зачем: токен из `api.token` — единственный секрет и живёт вечно. Когда ядро
// открыто в сеть (docs/pairing.md), нужен способ отозвать доступ у одного
// устройства, не меняя секрет всех остальных. Поэтому спаренный клиент получает
// СВОЙ токен, а в файле лежит только его SHA-256 — реестр не является набором
// действующих секретов.
//
// Совместимость: локальные сценарии (TUI, веб на том же хосте) продолжают ходить
// с токеном из `api.token`; реестр — дополнение, а не замена.

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::R;

/// Одно подключённое устройство.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Client {
    pub id: String,
    /// Имя от человека: «телефон», «ноутбук». Нужно, чтобы отзывать осознанно.
    pub name: String,
    /// SHA-256 токена в hex. Самого токена здесь нет и быть не должно.
    pub token_sha256: String,
    /// unix-время создания и последнего обращения.
    pub created_at: u64,
    #[serde(default)]
    pub last_seen: Option<u64>,
}

impl Client {
    /// Совпадает ли предъявленный токен с этим клиентом.
    pub fn matches(&self, token: &str) -> bool {
        // Сравнение постоянного времени: длина хеша фиксирована, поэтому утечка
        // через время сравнения тут была бы теоретической, но привычка дешёвая.
        let digest = hex(&Sha256::digest(token.as_bytes()));
        constant_time_eq(digest.as_bytes(), self.token_sha256.as_bytes())
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Каталог реестра: `<data>/clients`, по файлу на устройство.
pub fn dir(data: &Path) -> PathBuf {
    data.join("clients")
}

fn path_of(data: &Path, id: &str) -> PathBuf {
    dir(data).join(format!("{id}.json"))
}

/// Прочитать всех клиентов. Битый файл пропускается: реестр не должен ронять ход.
pub fn list(data: &Path) -> Vec<Client> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir(data)) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(client) = serde_json::from_str::<Client>(&text) {
                out.push(client);
            }
        }
    }
    out.sort_by_key(|c| c.created_at);
    out
}

/// Записать клиента атомарно (tmp + rename): половина файла — это клиент,
/// которого нельзя ни опознать, ни отозвать.
pub fn save(data: &Path, client: &Client) -> R<()> {
    std::fs::create_dir_all(dir(data))?;
    let path = path_of(data, &client.id);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(client)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 0600: там хеш токена. Файл не секрет, но и читать его чужим незачем.
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Отозвать устройство: удалить файл. `false` — такого не было.
pub fn revoke(data: &Path, id: &str) -> R<bool> {
    let path = path_of(data, id);
    if !path.is_file() {
        return Ok(false);
    }
    std::fs::remove_file(&path)?;
    Ok(true)
}

/// Генератор токена и id. 32 байта из `uuid` дважды — без лишних зависимостей,
/// 256 бит энтропии из ОС достаточно.
pub fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Кэш в памяти: проверка токена не должна читать каталог на каждый запрос.
#[derive(Default)]
pub struct Registry {
    clients: Mutex<Vec<Client>>,
}

impl Registry {
    pub fn load(data: &Path) -> Self {
        Self {
            clients: Mutex::new(list(data)),
        }
    }

    /// Перечитать с диска: после `snens pair` из другого процесса (CLI) или
    /// после отзыва через API список в памяти иначе остался бы старым.
    pub fn reload(&self, data: &Path) {
        if let Ok(mut guard) = self.clients.lock() {
            *guard = list(data);
        }
    }

    pub fn add(&self, client: Client) {
        if let Ok(mut guard) = self.clients.lock() {
            guard.retain(|c| c.id != client.id);
            guard.push(client);
        }
    }

    pub fn remove(&self, id: &str) {
        if let Ok(mut guard) = self.clients.lock() {
            guard.retain(|c| c.id != id);
        }
    }

    /// Кто это: `Some(id)` для известного клиента, `None` — токена нет в реестре.
    pub fn owner(&self, token: &str) -> Option<String> {
        let guard = self.clients.lock().ok()?;
        guard
            .iter()
            .find(|c| c.matches(token))
            .map(|c| c.id.clone())
    }

    pub fn snapshot(&self) -> Vec<Client> {
        self.clients
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }
}

/// Время сейчас, unix-секунды.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("snens-clients-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn client(id: &str, name: &str, token: &str) -> Client {
        Client {
            id: id.into(),
            name: name.into(),
            token_sha256: hex(&Sha256::digest(token.as_bytes())),
            created_at: now(),
            last_seen: None,
        }
    }

    #[test]
    fn token_is_matched_by_hash_and_never_stored_in_clear() {
        let dir = tmp();
        let c = client("c1", "телефон", "secret-token-1");
        save(&dir, &c).unwrap();
        // В файле нет самого токена — только хеш.
        let raw = std::fs::read_to_string(path_of(&dir, "c1")).unwrap();
        assert!(!raw.contains("secret-token-1"), "токен попал в файл: {raw}");
        assert!(raw.contains("token_sha256"));
        // Загруженный клиент узнаёт свой токен и не узнаёт чужой.
        let registry = Registry::load(&dir);
        assert_eq!(registry.owner("secret-token-1").as_deref(), Some("c1"));
        assert_eq!(registry.owner("other"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn revoke_removes_and_reload_picks_it_up() {
        let dir = tmp();
        save(&dir, &client("c1", "телефон", "t1")).unwrap();
        let registry = Registry::load(&dir);
        assert!(registry.owner("t1").is_some());
        assert!(revoke(&dir, "c1").unwrap());
        // В памяти ещё старый список — перечитка обязательна.
        registry.reload(&dir);
        assert_eq!(registry.owner("t1"), None);
        assert!(!revoke(&dir, "c1").unwrap(), "повторный отзыв — false");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broken_file_is_skipped_not_fatal() {
        let dir = tmp();
        save(&dir, &client("good", "ноутбук", "t")).unwrap();
        std::fs::write(dir.join("clients").join("bad.json"), "{не json").unwrap();
        let list = list(&dir);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "good");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tokens_are_unique_and_long_enough() {
        let a = new_token();
        let b = new_token();
        assert_ne!(a, b);
        assert!(a.len() >= 32, "токен короче 32 символов: {}", a.len());
    }
}
