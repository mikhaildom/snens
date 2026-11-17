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
        if !d.trim().is_empty() {
            return PathBuf::from(d);
        }
    }
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            // Без HOME состояние уезжало в /tmp/.local/state — общий каталог, где
            // чужой пользователь мог подложить target.json и увести привязку.
            // В таком случае берём каталог, принадлежащий только нам.
            match std::env::var("HOME") {
                Ok(h) if !h.trim().is_empty() => PathBuf::from(h).join(".local/state"),
                _ => PathBuf::from(format!(
                    "{}/snens-web-{}",
                    std::env::temp_dir().display(),
                    std::process::id()
                )),
            }
        });
    base.join("snens/web")
}

/// Атомарная запись: читатель не увидит половину файла, а падение посреди
/// записи не оставит обрезанный JSON (он загрузился бы как None и молча
/// выбросил привязку).
fn write_atomic(path: &PathBuf, body: &str) -> Result<(), String> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, body).map_err(|e| format!("state: {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("state: {}: {e}", path.display())
    })
}

/// Идентификатор вкладки приходит из CDP (и из target.json): как компонент пути
/// он должен быть простым — иначе `../..` уводит запись за каталог состояния.
fn safe_tab(tab: &str) -> String {
    tab.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[derive(Clone)]
pub struct Target {
    /// порт на нашей стороне (у туннеля — локальный конец)
    pub port: u16,
    /// порт на стороне Chrome: он в Host и в ws-URL
    pub remote_port: u16,
    /// ws-путь браузера: в режиме одобрения Chrome ≥144 `/json/*` не отдаёт, и
    /// единственный способ открыть новую вкладку — знать путь из DevToolsActivePort
    pub browser_path: Option<String>,
}

pub fn save_target(t: Target, tab: &str) -> Result<(), String> {
    let d = dir();
    fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    let v = serde_json::json!({
        "port": t.port,
        "remote_port": t.remote_port,
        "browser_path": t.browser_path,
        "tab": tab,
    });
    write_atomic(&d.join("target.json"), &v.to_string())
}

pub fn load_target() -> Option<(Target, String)> {
    let text = fs::read_to_string(dir().join("target.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    // `as u16` молча обрезал бы `"port": 70000` до 4464: испорченный файл должен
    // давать None, а не бодрый чужой порт.
    let port = u16::try_from(v.get("port").and_then(Value::as_u64)?).ok()?;
    let remote_port = match v.get("remote_port").and_then(Value::as_u64) {
        Some(p) => u16::try_from(p).ok()?,
        None => port,
    };
    let tab = v.get("tab").and_then(Value::as_str)?.to_string();
    let browser_path = v
        .get("browser_path")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some((
        Target {
            port,
            remote_port,
            browser_path,
        },
        tab,
    ))
}

/// Снимает привязку (например вкладку закрыли) — иначе следующий вызов будет
/// биться о мёртвый id.
pub fn clear_target() {
    let _ = fs::remove_file(dir().join("target.json"));
}

/// Живой ssh-туннель: чтобы следующий вызов (свой процесс) знал, через что идти.
pub fn save_tunnel(t: &crate::tunnel::Tunnel) -> Result<(), String> {
    let d = dir();
    fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    let v = serde_json::json!({
        "host": t.host,
        "local_port": t.local_port,
        "remote_port": t.remote_port,
        "pid": t.pid,
    });
    fs::write(d.join("tunnel.json"), v.to_string()).map_err(|e| format!("tunnel: {e}"))
}

pub fn load_tunnel() -> Option<crate::tunnel::Tunnel> {
    let text = fs::read_to_string(dir().join("tunnel.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    Some(crate::tunnel::Tunnel {
        host: v.get("host").and_then(Value::as_str)?.to_string(),
        local_port: v.get("local_port").and_then(Value::as_u64)? as u16,
        remote_port: v.get("remote_port").and_then(Value::as_u64)? as u16,
        pid: v.get("pid").and_then(Value::as_u64)? as u32,
    })
}

pub fn clear_tunnel() {
    let _ = fs::remove_file(dir().join("tunnel.json"));
}

/// Порт агентского Chrome на машине пользователя: следующий `launch` его
/// переиспользует, а не поднимает второй профиль.
pub fn save_launch(host: &str, port: u16) {
    let d = dir();
    let _ = fs::create_dir_all(&d);
    let v = serde_json::json!({"host": host, "port": port});
    let _ = fs::write(d.join("launch.json"), v.to_string());
}

pub fn load_launch() -> Option<(String, u16)> {
    let text = fs::read_to_string(dir().join("launch.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    Some((
        v.get("host").and_then(Value::as_str)?.to_string(),
        u16::try_from(v.get("port").and_then(Value::as_u64)?).ok()?,
    ))
}

pub fn save_refs(tab: &str, refs: &[Ref]) -> Result<PathBuf, String> {
    let tab = &safe_tab(tab);
    let d = dir().join(tab);
    fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    let p = d.join("refs.json");
    let text = serde_json::to_string(refs).map_err(|e| format!("refs: {e}"))?;
    write_atomic(&p, &text)?;
    Ok(p)
}

pub fn load_refs(tab: &str) -> Result<Vec<Ref>, String> {
    let tab = &safe_tab(tab);
    let p = dir().join(tab).join("refs.json");
    let text = fs::read_to_string(&p)
        .map_err(|e| format!("нет адресов для вкладки {tab} ({}): сначала `web snap`", e))?;
    serde_json::from_str(&text).map_err(|e| format!("refs: {e}"))
}

/// URL, к которому относится последний снимок: по нему отличаем «страница та
/// же» от «мы уже на другой, просто снимок сделан про старую».
pub fn save_url(tab: &str, url: &str) {
    let d = dir().join(safe_tab(tab));
    let _ = fs::create_dir_all(&d);
    let _ = write_atomic(&d.join("url.txt"), url);
}

pub fn load_url(tab: &str) -> String {
    fs::read_to_string(dir().join(safe_tab(tab)).join("url.txt")).unwrap_or_default()
}

pub fn save_page(tab: &str, text: &str) -> Result<PathBuf, String> {
    let d = dir().join(safe_tab(tab));
    fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    let p = d.join("page.txt");
    write_atomic(&p, text)?;
    Ok(p)
}

pub fn load_page(tab: &str) -> Option<String> {
    fs::read_to_string(dir().join(safe_tab(tab)).join("page.txt")).ok()
}
