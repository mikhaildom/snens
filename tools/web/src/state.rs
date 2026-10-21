// Состояние между вызовами: к какому порту и вкладке мы привязаны, и какие
// адреса (`e12`) в какой вкладке живут. Демон stateless по процессу — всё
// нужное лежит файлами, поэтому `web act` работает отдельным вызовом, в том
// числе через ssh.
use std::fs;
use std::path::PathBuf;

use serde_json::Value;

use crate::snap::Ref;

pub fn dir() -> PathBuf {
    if let Ok(d) = std::env::var("SNENS_WEB_STATE") {
        return PathBuf::from(d);
    }
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".local/state")
        });
    base.join("snens/web")
}

#[derive(Clone, Copy)]
pub struct Target {
    pub port: u16,
}

pub fn save_target(t: Target, tab: &str) -> Result<(), String> {
    let d = dir();
    fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    let v = serde_json::json!({"port": t.port, "tab": tab});
    fs::write(d.join("target.json"), v.to_string()).map_err(|e| format!("state: {e}"))
}

pub fn load_target() -> Option<(Target, String)> {
    let text = fs::read_to_string(dir().join("target.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let port = v.get("port").and_then(Value::as_u64)? as u16;
    let tab = v.get("tab").and_then(Value::as_str)?.to_string();
    Some((Target { port }, tab))
}

/// Снимает привязку (например вкладку закрыли) — иначе следующий вызов будет
/// биться о мёртвый id.
pub fn clear_target() {
    let _ = fs::remove_file(dir().join("target.json"));
}

pub fn save_refs(tab: &str, refs: &[Ref]) -> Result<PathBuf, String> {
    let d = dir().join(tab);
    fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    let p = d.join("refs.json");
    let text = serde_json::to_string(refs).map_err(|e| format!("refs: {e}"))?;
    fs::write(&p, text).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(p)
}

pub fn load_refs(tab: &str) -> Result<Vec<Ref>, String> {
    let p = dir().join(tab).join("refs.json");
    let text = fs::read_to_string(&p).map_err(|e| {
        format!(
            "нет адресов для вкладки {tab} ({}): сначала `web snap`",
            e
        )
    })?;
    serde_json::from_str(&text).map_err(|e| format!("refs: {e}"))
}

/// URL, к которому относится последний снимок: по нему отличаем «страница та
/// же» от «мы уже на другой, просто снимок сделан про старую».
pub fn save_url(tab: &str, url: &str) {
    let d = dir().join(tab);
    let _ = fs::create_dir_all(&d);
    let _ = fs::write(d.join("url.txt"), url);
}

pub fn load_url(tab: &str) -> String {
    fs::read_to_string(dir().join(tab).join("url.txt")).unwrap_or_default()
}

pub fn save_page(tab: &str, text: &str) -> Result<PathBuf, String> {
    let d = dir().join(tab);
    fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    let p = d.join("page.txt");
    fs::write(&p, text).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(p)
}

pub fn load_page(tab: &str) -> Option<String> {
    fs::read_to_string(dir().join(tab).join("page.txt")).ok()
}
