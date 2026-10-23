// Ящик: сообщение = файл в `<mail>/<id>/new/`, прочитанное уезжает в `read/`.
// Формат общий с ядром (src/mail.rs): {ts, from, kind, text}, kind — `steer` |
// `ask` | `report`. Здесь же — топология дерева: дети, родитель, братья.
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::log::now;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Msg {
    pub ts: u64,
    pub from: String,
    pub kind: String,
    pub text: String,
}

pub fn sanitize(s: &str) -> String {
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

pub fn box_dir(mail: &Path, id: &str) -> PathBuf {
    mail.join(id)
}

pub fn new_dir(mail: &Path, id: &str) -> PathBuf {
    box_dir(mail, id).join("new")
}

/// Положить сообщение `to`. Имя файла сортируется по времени — порядок доставки
/// детерминирован; запись атомарна (временный файл + rename).
pub fn send(mail: &Path, to: &str, from: &str, kind: &str, text: &str) {
    let dir = new_dir(mail, to);
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| crate::die(format!("agent: ящик {to}: {e}")));
    let msg = Msg {
        ts: now(),
        from: from.to_string(),
        kind: kind.to_string(),
        text: text.to_string(),
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = format!("{nanos:020}-{}-{}.json", sanitize(from), std::process::id());
    let path = dir.join(&name);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string(&msg).unwrap_or_default())
        .unwrap_or_else(|e| crate::die(format!("agent: запись сообщения: {e}")));
    let _ = std::fs::rename(&tmp, &path);
}

/// Забрать только те входящие, что проходят фильтр: они уезжают в `read/` (ровно
/// один потребитель). Остальные **остаются в `new/`** — фильтр, а не «забрать всё
/// и вернуть обратно»: возврат переупорядочивал бы их и мог зациклиться, если в
/// ящике лежит то, что мы сами же не считаем ответом (kind=ask).
pub fn drain_filter(mail: &Path, id: &str, keep: impl Fn(&Msg) -> bool) -> Vec<Msg> {
    let dir = new_dir(mail, id);
    let done = box_dir(mail, id).join("read");
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
        let Ok(m) = serde_json::from_str::<Msg>(&text) else {
            // Битый файл — не повод ронять вызов: выносим и идём дальше.
            let _ = std::fs::remove_file(&p);
            continue;
        };
        if !keep(&m) {
            continue;
        }
        let _ = std::fs::create_dir_all(&done);
        let _ = std::fs::rename(&p, done.join(p.file_name().unwrap_or_default()));
        out.push(m);
    }
    out
}

/// Посмотреть входящие, не забирая их: «есть ли вопрос» спрашивают, не имея права
/// съесть сообщение — его должен прочитать ход сессии.
pub fn peek(mail: &Path, id: &str) -> Vec<Msg> {
    let mut names: Vec<PathBuf> = match std::fs::read_dir(new_dir(mail, id)) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
            .collect(),
        Err(_) => return Vec::new(),
    };
    names.sort();
    names
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .filter_map(|t| serde_json::from_str::<Msg>(&t).ok())
        .collect()
}

fn child_ids(mail: &Path, parent: &str) -> Vec<String> {
    let mut ids: Vec<String> = match std::fs::read_dir(box_dir(mail, parent).join("children")) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.path().is_file())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect(),
        Err(_) => Vec::new(),
    };
    ids.sort();
    ids
}

/// Дети сессии (то, что она запустила): `<mail>/<id>/children/<child>` = pid.
pub fn children(mail: &Path, id: &str) -> Vec<String> {
    child_ids(mail, id)
}

/// Обратный поиск: чей `children/` содержит `me`. Он и есть настоящий родитель,
/// если SNENS_PARENT пуст (тул запущен руками или переменную потеряли).
pub fn parent_of(mail: &Path, me: &str) -> Option<String> {
    let mut boxes: Vec<String> = match std::fs::read_dir(mail) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect(),
        Err(_) => return None,
    };
    boxes.sort();
    boxes
        .into_iter()
        .find(|b| b != me && box_dir(mail, b).join("children").join(me).is_file())
}

/// Братья: другие дети того же родителя.
pub fn siblings(mail: &Path, parent: &str, me: &str) -> Vec<String> {
    child_ids(mail, parent)
        .into_iter()
        .filter(|c| c != me)
        .collect()
}
