// Снимок дерева доступности → компактный текст с адресами (`e12`) и таблица
// «адрес → узел» для действий.
//
// Почему так, а не сырой AX JSON: полное дерево страницы — 500–600 КБ JSON
// (замер на Chrome 153), сжатие по интерактивным узлам — 10–16 КБ, то есть в
// 30–60 раз дешевле. В контекст модели уезжает не дерево, а путь к файлу и
// счётчики: читает она нужное через grep/read.
use std::collections::HashMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cdp::Cdp;

/// Роли, по которым можно действовать. Всё остальное остаётся, только если
/// несёт текст (static text) — иначе дерево раздувается служебными узлами.
const INTERACTIVE: &[&str] = &[
    "button",
    "link",
    "textbox",
    "searchbox",
    "checkbox",
    "radio",
    "combobox",
    "listbox",
    "option",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "tab",
    "switch",
    "slider",
    "spinbutton",
    "treeitem",
    "disclosureTriangle",
    "toggleButton",
    "popupButton",
];

#[derive(Clone, Serialize, Deserialize)]
pub struct Ref {
    pub addr: String,
    /// `backendDOMNodeId` — адрес узла в DOM, по нему кликаем и вводим текст.
    pub node: i64,
    pub role: String,
    pub name: String,
}

#[derive(Default)]
pub struct Snap {
    pub text: String,
    pub refs: Vec<Ref>,
    pub total: usize,
    pub kept: usize,
    pub title: String,
    pub url: String,
}

/// FNV-1a по адресам, ролям и тексту дерева: дешёвый детерминированный
/// отпечаток, не зависящий от версии компилятора (в отличие от DefaultHasher).
fn fingerprint(s: &Snap) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
    };
    eat(b"addr");
    for r in &s.refs {
        eat(r.addr.as_bytes());
        eat(b"\0");
    }
    eat(b"tree");
    eat(s.text.as_bytes());
    h
}

/// Снимок с ожиданием стабилизации. AX-дерево обновляется асинхронно: сразу
/// после навигации Chrome ещё отдаёт дерево предыдущего документа при том, что
/// `location.href` уже новый (наблюдали: после логина снимок показывал форму
/// входа на странице товаров). Поэтому снимаем до трёх раз, пока не совпадут
/// размер и набор ролей.
pub fn take_stable(cdp: &mut Cdp, depth_cap: usize) -> Result<Snap, String> {
    let mut prev: Option<(usize, usize, String, u64)> = None;
    let mut last: Option<Snap> = None;
    for attempt in 0..4 {
        let s = take(cdp, depth_cap)?;
        // Отпечаток самого дерева, а не только счётчиков: две разные страницы с
        // одинаковым числом узлов и тем же URL считались «стабильными», и в act
        // уезжали адреса предыдущего рендера.
        let sig = (s.total, s.refs.len(), s.url.clone(), fingerprint(&s));
        if let Some(p) = &prev {
            if *p == sig {
                return Ok(s);
            }
        }
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(180));
        }
        prev = Some(sig);
        last = Some(s);
    }
    Ok(last.unwrap_or_default())
}

pub fn take(cdp: &mut Cdp, depth_cap: usize) -> Result<Snap, String> {
    let _ = cdp.call("Accessibility.enable", Value::Null);
    let r = cdp.call("Accessibility.getFullAXTree", Value::Null)?;
    let nodes = r
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut by_id: HashMap<String, &Value> = HashMap::new();
    for n in &nodes {
        if let Some(id) = n.get("nodeId").and_then(Value::as_str) {
            by_id.insert(id.to_string(), n);
        }
    }
    // Корни — узлы, на которые никто не ссылается как на ребёнка.
    let mut claimed: HashMap<&str, ()> = HashMap::new();
    for n in &nodes {
        for c in n
            .get("childIds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(s) = c.as_str() {
                claimed.insert(s, ());
            }
        }
    }
    let mut snap = Snap {
        total: nodes.len(),
        ..Default::default()
    };
    snap.title = cdp
        .eval("document.title")
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    snap.url = cdp
        .eval("location.href")
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();

    let mut counter = 0usize;
    for n in &nodes {
        let id = n.get("nodeId").and_then(Value::as_str).unwrap_or("");
        if claimed.contains_key(id) || id.is_empty() {
            continue;
        }
        walk(n, &by_id, 0, depth_cap, &mut counter, &mut snap);
    }
    snap.kept = snap.refs.len();
    Ok(snap)
}

fn role_of(n: &Value) -> String {
    n.get("role")
        .and_then(|r| r.get("value"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn name_of(n: &Value) -> String {
    n.get("name")
        .and_then(|r| r.get("value"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn ignored(n: &Value) -> bool {
    n.get("ignored").and_then(Value::as_bool).unwrap_or(false)
}

/// Состояние — только то, что меняет решение: выключено, отмечено, раскрыто,
/// в фокусе. Остальные свойства в дерево не тащим.
fn state_of(n: &Value) -> Vec<&'static str> {
    let mut out = Vec::new();
    let props = n.get("properties").and_then(Value::as_array);
    for p in props.into_iter().flatten() {
        let name = p.get("name").and_then(Value::as_str).unwrap_or("");
        let v = p.get("value").and_then(|v| v.get("value")).cloned();
        match (name, v) {
            ("disabled", Some(Value::Bool(true))) => out.push("disabled"),
            ("checked", Some(Value::Bool(true))) => out.push("checked"),
            ("expanded", Some(Value::Bool(true))) => out.push("expanded"),
            ("focused", Some(Value::Bool(true))) => out.push("focused"),
            ("invalid", Some(Value::Bool(true))) => out.push("invalid"),
            ("required", Some(Value::Bool(true))) => out.push("required"),
            ("selected", Some(Value::Bool(true))) => out.push("selected"),
            _ => {}
        }
    }
    out
}

fn line_for(n: &Value, addr: Option<&str>, depth: usize) -> Option<String> {
    let role = role_of(n);
    let name = name_of(n);
    let value = n
        .get("value")
        .and_then(|r| r.get("value"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let interactive = INTERACTIVE.contains(&role.as_str());
    let has_text = !name.is_empty() || !value.is_empty();
    if role.is_empty() || (!interactive && !has_text) {
        return None;
    }
    let mut s = String::new();
    let _ = write!(s, "{}{}", "  ".repeat(depth), role);
    if let Some(a) = addr {
        let _ = write!(s, " [{a}]");
    }
    if !name.is_empty() {
        let _ = write!(s, " \"{}\"", short(&name, 120));
    }
    if !value.is_empty() {
        let _ = write!(s, " = \"{}\"", short(&value, 80));
    }
    for st in state_of(n) {
        let _ = write!(s, " ({st})");
    }
    Some(s)
}

fn short(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n {
        one
    } else {
        let t: String = one.chars().take(n).collect();
        format!("{t}…")
    }
}

fn walk(
    n: &Value,
    by_id: &HashMap<String, &Value>,
    depth: usize,
    depth_cap: usize,
    counter: &mut usize,
    snap: &mut Snap,
) {
    // Служебные узлы (`ignored`) сами не печатаем, но их детей обходим: в Chrome
    // почти вся полезная начинка висит под `none`/`generic`-обёртками, и обрыв
    // на них давал пустое дерево (61 узел, ни одной кнопки).
    if depth > depth_cap {
        return;
    }
    if ignored(n) {
        for c in n
            .get("childIds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(child) = c.as_str().and_then(|cid| by_id.get(cid)) {
                // Шаг глубины на pass-through узле: иначе цепочка ignored-обёрток
                // не продвигалась и depth_cap не ограничивал рекурсию вовсе.
                walk(child, by_id, depth + 1, depth_cap, counter, snap);
            }
        }
        return;
    }
    let role = role_of(n);
    let interactive = INTERACTIVE.contains(&role.as_str());
    // Адрес даём только интерактивным: адрес на текстовом узле бесполезен,
    // а номер в дереве сбивает модель («кликни по e12» там, где кликать нечего).
    let addr = if interactive {
        let node = n
            .get("backendDOMNodeId")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if node == 0 {
            None
        } else {
            *counter += 1;
            let a = format!("e{counter}");
            snap.refs.push(Ref {
                addr: a.clone(),
                node,
                role: role.clone(),
                name: name_of(n),
            });
            Some(a)
        }
    } else {
        None
    };
    let text_len = snap.text.len();
    if let Some(l) = line_for(n, addr.as_deref(), depth) {
        snap.text.push_str(&l);
        snap.text.push('\n');
    }
    let child_depth = if snap.text.len() > text_len {
        depth + 1
    } else {
        depth
    };
    for c in n
        .get("childIds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(cid) = c.as_str() {
            if let Some(child) = by_id.get(cid) {
                walk(child, by_id, child_depth, depth_cap, counter, snap);
            }
        }
    }
}
