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
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let local = if cfg!(windows) { var("LOCALAPPDATA") } else { None };
    base_dir(var("XDG_STATE_HOME"), var("HOME"), local).join("snens/web")
}

/// Где лежит состояние, по уже прочитанным переменным (пустые — `None`).
///
/// Без HOME состояние уезжало в /tmp/.local/state — общий каталог, где чужой
/// пользователь мог подложить target.json и увести привязку. В таком случае
/// берём каталог, принадлежащий только нам.
///
/// На Windows HOME обычно нет вовсе, и каждый вызов `web` получал свой каталог
/// по pid — привязка из `connect`/`launch` терялась уже к следующему вызову.
/// Там каталог пользователя — LOCALAPPDATA (его передают только на Windows), и он
/// важнее HOME: ssh и Git Bash HOME задают, обычное окно — нет, а привязка должна
/// быть одна на оба пути.
fn base_dir(xdg: Option<String>, home: Option<String>, local: Option<String>) -> PathBuf {
    if let Some(x) = xdg {
        return PathBuf::from(x);
    }
    if let Some(l) = local {
        return PathBuf::from(l);
    }
    if let Some(h) = home {
        return PathBuf::from(h).join(".local/state");
    }
    PathBuf::from(format!(
        "{}/snens-web-{}",
        std::env::temp_dir().display(),
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn base_dir_order_and_stable_without_home() {
        assert_eq!(base_dir(s("/x"), s("/h"), s("C:/l")), PathBuf::from("/x"));
        assert_eq!(base_dir(None, s("/h"), None), PathBuf::from("/h").join(".local/state"));
        // Windows: каталог пользователя, с HOME и без — один и тот же.
        assert_eq!(base_dir(None, s("/h"), s("C:/l")), PathBuf::from("C:/l"));
        assert_eq!(base_dir(None, None, s("C:/l")), PathBuf::from("C:/l"));
        assert_eq!(base_dir(None, None, s("C:/l")), base_dir(None, None, s("C:/l")));
        // Совсем ничего — свой каталог процесса, не общий /tmp/.local/state.
        let p = base_dir(None, None, None);
        assert!(p.to_string_lossy().contains(&format!("snens-web-{}", std::process::id())));
    }
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
    let text = match fs::read_to_string(&p) {
        Ok(t) => t,
        // «Нет снимка» и «снимок не читается» — разные болезни: во втором случае
        // совет сделать snap не поможет.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "нет адресов для вкладки {tab} ({}): сначала `web snap`",
                p.display()
            ))
        }
        Err(e) => return Err(format!("{}: {e}", p.display())),
    };
    serde_json::from_str(&text)
        .map_err(|e| format!("{}: {e} — сделай `web snap` заново", p.display()))
}

/// URL, к которому относится последний снимок: по нему отличаем «страница та
/// же» от «мы уже на другой, просто снимок сделан про старую».
/// Сохранить URL последнего снимка.
pub fn save_url(tab: &str, url: &str) {
    let d = dir().join(safe_tab(tab));
    let _ = fs::create_dir_all(&d);
    let _ = write_atomic(&d.join("url.txt"), url);
}

/// URL последнего снимка. `None` — файла нет; ошибку чтения (права, EISDIR)
/// тоже отдаём наружу: молча пустая строка меняла поведение `settle()`.
pub fn load_url(tab: &str) -> Option<String> {
    match fs::read_to_string(dir().join(safe_tab(tab)).join("url.txt")) {
        Ok(u) => Some(u),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            eprintln!("state: url.txt не читается: {e}");
            None
        }
    }
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
