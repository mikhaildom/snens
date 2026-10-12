//! Experimental entrypoint. Never installs or rebuilds the production binary.
use crate::{cfg::Config, R};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

const HELP: &str = "snens-next — experimental surface API (separate data and binary)\n\
  serve --config PATH [--data-dir DIR] [--listen 127.0.0.1:PORT] [--token-file PATH]\n\
  tui [--endpoint http://127.0.0.1:7419] [--token-file PATH] [--session ID] [--data-dir DIR]\n\
Default data: ~/.local/share/snens-next; default token file: <data-dir>/api.token.\n\
--token-file auto uses <data-dir>/api.token; other values are literal paths.\n\
Start serve explicitly in another terminal. /quit disconnects; /stop cancels a run.\n\
Experimental: text/steer/stop/history only; no attachment upload or remote secret entry yet.\n";

pub async fn entry() -> R<()> {
    if matches!(
        crate::askpass::invoked_as().as_str(),
        "sudo" | "snens-askpass"
    ) {
        return crate::legacy_main().await;
    }
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cmd = argv.first().map(String::as_str).unwrap_or("help");
    // Detached children must inherit the isolated runtime config. Never expose
    // build/improve/model here: those can overwrite the working installation.
    if matches!(
        cmd,
        "run" | "run-child" | "steer" | "agents" | "result" | "show" | "log" | "review"
    ) {
        if std::env::var_os("SNENS_NEXT_INTERNAL").is_none() {
            return Err("internal command: use snens-next tui for the API".into());
        }
        return crate::legacy_main().await;
    }
    if matches!(cmd, "help" | "--help" | "-h") {
        print!("{HELP}");
        return Ok(());
    }
    let flags = flags(&argv[1..])?;
    let data = PathBuf::from(
        flags
            .get("data-dir")
            .cloned()
            .unwrap_or_else(|| format!("{}-next", crate::cfg::data_dir())),
    );
    let data = prepare_data(&data)?;
    let token_path = token_path_for(flags.get("token-file"), &data);
    match cmd {
        "serve" => {
            let config_path = flags.get("config").ok_or(
                "serve requires --config PATH (production config is never used implicitly)",
            )?;
            use fs2::FileExt;
            let launch_lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(data.join("launcher.lock"))?;
            launch_lock
                .try_lock_exclusive()
                .map_err(|_| "snens-next already serving this data directory")?;
            let cfg = isolate(config_path, &data)?;
            let token = token(&token_path, true)?;
            let addr = flags
                .get("listen")
                .map(String::as_str)
                .unwrap_or("127.0.0.1:7419")
                .parse::<std::net::SocketAddr>()?;
            if !addr.ip().is_loopback() {
                return Err("experimental server only binds loopback".into());
            }
            eprintln!(
                "[snens-next] http://{addr} · data {} · token {}",
                data.display(),
                token_path.display()
            );
            crate::surface_server::serve(cfg, addr, token).await
        }
        "tui" => {
            let token = token(&token_path, false)?;
            let endpoint = flags
                .get("endpoint")
                .cloned()
                .unwrap_or_else(|| "http://127.0.0.1:7419".into());
            crate::surface_tui::run(
                endpoint,
                token,
                flags.get("session").cloned(),
                data.join("history.txt"),
            )
            .await
        }
        _ => Err(format!("unknown command {cmd}\n{HELP}").into()),
    }
}

/// Сантинел `auto` — не путь, а просьба положить токен в data-dir: `./auto`
/// в cwd неожиданно для запускающего создавал секрет в чужом каталоге.
fn token_path_for(flag: Option<&String>, data: &Path) -> PathBuf {
    match flag.map(String::as_str) {
        Some("auto") | None => data.join("api.token"),
        Some(path) => PathBuf::from(path),
    }
}

fn flags(args: &[String]) -> R<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let mut args = args.iter();
    while let Some(key) = args.next() {
        if !matches!(
            key.as_str(),
            "--config" | "--data-dir" | "--listen" | "--token-file" | "--endpoint" | "--session"
        ) {
            return Err(format!("unknown option {key}").into());
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {key}"))?;
        if out
            .insert(key.trim_start_matches("--").into(), value.clone())
            .is_some()
        {
            return Err(format!("duplicate option {key}").into());
        }
    }
    Ok(out)
}

// Resolve existing ancestors before any mkdir, including symlinks into production.
fn future_path(path: &Path) -> R<PathBuf> {
    if path.exists() {
        return Ok(std::fs::canonicalize(path)?);
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = absolute.parent().ok_or("data path has no parent")?;
    let name = absolute.file_name().ok_or("invalid data path")?;
    Ok(future_path(parent)?.join(name))
}

fn prepare_data(path: &Path) -> R<PathBuf> {
    let data = future_path(path)?;
    let production = future_path(Path::new(&crate::cfg::data_dir()))?;
    if data.starts_with(production) {
        return Err("snens-next data must be outside the production snens instance".into());
    }
    std::fs::create_dir_all(&data)?;
    let data = std::fs::canonicalize(data)?;
    protect_data(&data)?;
    Ok(data)
}

fn protect_data(data: &Path) -> R<()> {
    let production = PathBuf::from(crate::cfg::data_dir());
    let production = std::fs::canonicalize(&production).unwrap_or(production);
    if data.starts_with(&production) {
        return Err("snens-next data must be outside the production snens instance".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(data, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn token(path: &Path, create: bool) -> R<String> {
    if create && !path.exists() {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(path)?;
        writeln!(
            file,
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        )?;
        file.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
            return Err(
                format!("token file {} must be private (chmod 600)", path.display()).into(),
            );
        }
    }
    let value = std::fs::read_to_string(path)?.trim().to_string();
    if value.len() < 32 || value.chars().any(char::is_whitespace) {
        return Err("API token must have >=32 characters and no whitespace".into());
    }
    Ok(value)
}

fn isolate(source: &str, data: &Path) -> R<Config> {
    let text = std::fs::read_to_string(source)?;
    let mut doc: toml::Table = toml::from_str(&text)?;
    let original = Config::load(Some(source))?;
    // Resolve the source config's private .env before switching to runtime.toml.
    // Detached children inherit the resolved key, even with a custom data-dir.
    std::env::set_var("SNENS_CONFIG", std::fs::canonicalize(source)?);
    if let Ok(key) = original.api_key() {
        std::env::set_var(&original.key_env, key);
    }
    // Separate bin directory: registry::sync_bin_dir mutates it on every run.
    let tools = data.join("tools");
    std::fs::create_dir_all(&tools)?;
    if original.tools_path().is_dir() {
        for entry in std::fs::read_dir(original.tools_path())? {
            let entry = entry?;
            if !entry.path().is_dir() {
                continue;
            }
            let target = std::fs::canonicalize(entry.path())?;
            let link = tools.join(entry.file_name());
            if link.exists() {
                continue;
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, link)?;
            #[cfg(not(unix))]
            {
                let _ = (target, link);
                return Err("tool staging currently requires Unix".into());
            }
        }
    }
    for (key, name) in [
        ("sessions_dir", "sessions"),
        ("mail_dir", "mail"),
        ("tickets_dir", "tickets"),
        ("tools_dir", "tools"),
    ] {
        doc.insert(
            key.into(),
            toml::Value::String(data.join(name).display().to_string()),
        );
    }
    // Память экспериментального прогона не трогает общий банк: канал выключен.
    doc.insert("memory".into(), toml::Value::Table(toml::Table::new()));
    // Авторевью управляется самим runtime.toml. Раньше экспериментальный инстанс
    // глушил его `review_min_turns = MAX`; при снятом изоляционном конфиге этот
    // ключ остаётся в source и выключил бы триггер — снимаем его, берётся дефолт.
    // Ревью читает и пишет только в изолированный data-dir, производства не касается.
    doc.remove("review_min_turns");
    let runtime = data.join("runtime.toml");
    std::fs::write(&runtime, toml::to_string_pretty(&doc)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o600))?;
    }
    std::env::set_var("SNENS_CONFIG", &runtime);
    std::env::set_var("SNENS_NEXT_INTERNAL", "1");
    let cfg = Config::load(runtime.to_str())?;
    let shim = crate::askpass::ensure_shims(&cfg)?;
    #[cfg(unix)]
    {
        let link = shim.join("snens");
        if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link)?;
        }
        std::os::unix::fs::symlink(crate::self_exe()?, link)?;
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn token_path_resolves_sentinel_to_data_dir() {
        let data = Path::new("/tmp/never-created-snens-next");
        assert_eq!(token_path_for(None, data), data.join("api.token"));
        assert_eq!(
            token_path_for(Some(&"auto".to_string()), data),
            data.join("api.token")
        );
        assert_eq!(
            token_path_for(Some(&"/tmp/explicit-token".to_string()), data),
            PathBuf::from("/tmp/explicit-token")
        );
    }
    #[test]
    fn strict_options() {
        assert!(flags(&["--unknown".into(), "x".into()]).is_err());
        assert!(flags(&["--config".into()]).is_err());
        assert!(flags(&[
            "--session".into(),
            "a".into(),
            "--session".into(),
            "b".into()
        ])
        .is_err());
    }
    #[test]
    fn production_data_is_rejected() {
        let path = PathBuf::from(crate::cfg::data_dir());
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        assert!(protect_data(&path).is_err());
        assert!(protect_data(&path.join("experiment")).is_err());
    }
    #[test]
    fn rejected_data_is_not_created() {
        let path = PathBuf::from(crate::cfg::data_dir())
            .join(format!("rejected-next-{}", uuid::Uuid::new_v4()));
        assert!(prepare_data(&path).is_err());
        assert!(!path.exists());
        #[cfg(unix)]
        {
            let temp = std::env::temp_dir().join(format!("next-path-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&temp).unwrap();
            std::os::unix::fs::symlink(crate::cfg::data_dir(), temp.join("alias")).unwrap();
            assert!(prepare_data(&temp.join("alias").join("forbidden")).is_err());
            std::fs::remove_dir_all(temp).unwrap();
        }
    }
    #[test]
    fn token_is_persistent_and_private() {
        let dir = std::env::temp_dir().join(format!("snens-next-token-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("token");
        let first = token(&path, true).unwrap();
        assert_eq!(first.len(), 64);
        assert_eq!(token(&path, false).unwrap(), first);
        assert_eq!(token(&path, true).unwrap(), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(token(&path, false).is_err());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
