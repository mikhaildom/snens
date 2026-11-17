// Ящик сессии — канал между подагентом и родителем (и steer человека).
// Сообщение — отдельный файл, а не строка в общем журнале: в ящик пишут два
// писателя (родитель и ребёнок), и общий файл дал бы гонку и offset-трекинг.
// Ядро знает только этот расклад; отправитель/получатель — любая сессия.
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::R;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Msg {
    pub ts: u64,
    /// Откуда пришло: id сессии или "human".
    pub from: String,
    /// `steer` | `ask` | `report` — что это за слово.
    pub kind: String,
    pub text: String,
}

fn box_dir(mail_dir: &Path, id: &str) -> PathBuf {
    mail_dir.join(id)
}

fn new_dir(mail_dir: &Path, id: &str) -> PathBuf {
    box_dir(mail_dir, id).join("new")
}

/// Положить сообщение получателю `to`. Писатель — отдельный файл, имя сортируется
/// по времени: порядок доставки детерминирован.
pub fn send(mail_dir: &Path, to: &str, from: &str, kind: &str, text: &str) -> R<()> {
    let dir = new_dir(mail_dir, to);
    std::fs::create_dir_all(&dir)?;
    let msg = Msg {
        ts: crate::log::now(),
        from: from.to_string(),
        kind: kind.to_string(),
        text: text.to_string(),
    };
    let name = format!("{:020}-{}-{}.json", nanots(), sanitize(from), nonce());
    let path = dir.join(name);
    // Пишем через временный файл + rename: читатель не увидит недописанное.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string(&msg)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Есть ли что-то непрочитанное — без изъятия из ящика (для ожидания в цикле).
pub fn has_pending(mail_dir: &Path, id: &str) -> bool {
    std::fs::read_dir(new_dir(mail_dir, id))
        .map(|rd| {
            rd.flatten()
                .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        })
        .unwrap_or(false)
}

/// Есть ли в ящике слово человека (`steer` от "human" или сообщение с его id).
/// Только такое пробуждение оправдано для любой сессии: это не внутренняя
/// переписка агентов.
pub fn has_human(mail_dir: &Path, id: &str) -> bool {
    let Ok(rd) = std::fs::read_dir(new_dir(mail_dir, id)) else {
        return false;
    };
    rd.flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|b| serde_json::from_slice::<Msg>(&b).ok())
        .any(|m| m.from == "human")
}

/// Вычитать и забрать входящие: переносит их в `read/` (ровно один потребитель).
pub fn drain(mail_dir: &Path, id: &str) -> Vec<Msg> {
    let dir = new_dir(mail_dir, id);
    let done = box_dir(mail_dir, id).join("read");
    let mut names: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
            .collect(),
        Err(_) => return Vec::new(),
    };
    names.sort();
    let mut out = Vec::new();
    for p in names {
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(msg) = serde_json::from_str::<Msg>(&text) else {
            // Битый файл — не повод ронять ход: выносим и идём дальше.
            let _ = std::fs::remove_file(&p);
            continue;
        };
        let _ = std::fs::create_dir_all(&done);
        let _ = std::fs::rename(&p, done.join(p.file_name().unwrap_or_default()));
        out.push(msg);
    }
    out
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn nanots() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn nonce() -> u64 {
    // Процесс + время наносекунд уникальны на практике; коллизии не различаем.
    std::process::id() as u64 ^ (nanots() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_then_drain_is_fifo_and_single_consume() {
        let dir = std::env::temp_dir().join(format!("snens-mail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        send(&dir, "child", "parent", "ask", "первое").unwrap();
        send(&dir, "child", "parent", "steer", "второе").unwrap();
        let got = drain(&dir, "child");
        assert_eq!(got.len(), 2, "оба сообщения должны дойти");
        assert_eq!(got[0].kind, "ask");
        assert!(got[0].text.contains("первое"));
        // Повторный дренаж пуст: сообщения ушли в read/, а не остались в new/.
        assert!(
            drain(&dir, "child").is_empty(),
            "сообщение доставлено дважды"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn has_pending_sees_unread_without_consuming() {
        let dir = std::env::temp_dir().join(format!("snens-mail-p-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!has_pending(&dir, "me"));
        send(&dir, "me", "child", "report", "итог").unwrap();
        assert!(has_pending(&dir, "me"));
        // has_pending ничего не забрал: сообщение всё ещё доступно дренажу.
        assert_eq!(drain(&dir, "me").len(), 1);
        assert!(!has_pending(&dir, "me"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
