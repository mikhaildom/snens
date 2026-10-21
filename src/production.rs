use crate::{cfg::Config, R};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

const HELP: &str = "snens — production entry\n\
  serve [--config PATH] [--listen 127.0.0.1:7420] [--token-file PATH]\n\
  tui [--config PATH] [--endpoint URL] [--token-file PATH] [--session ID]\n\
  tui-local [--session ID] [--config PATH] — прежний локальный TUI\n\
  run, run-child, review, tools, build, config/model и остальные команды — legacy CLI\n\nDefault config: ~/.config/snens/config.toml; API token: <cfg.data_path()>/api.token.\n\
The HTTP/SSE server binds loopback only. API TUI does not accept remote sudo secrets.\n";

pub async fn entry() -> R<()> {
    // Preserve executable aliases used by sudo/askpass shims.
    if matches!(
        crate::askpass::invoked_as().as_str(),
        "sudo" | "snens-askpass"
    ) {
        return crate::legacy_main().await;
    }
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cmd = argv.first().map(String::as_str).unwrap_or("help");
    match cmd {
        "serve" => {
            let parsed = flags(&argv[1..])?;
            validate_options(&parsed, &["config", "listen", "token-file"])?;
            serve(&parsed).await
        }
        "tui" => {
            let parsed = flags(&argv[1..])?;
            validate_options(&parsed, &["config", "endpoint", "token-file", "session"])?;
            set_effective_config(parsed.get("config").map(String::as_str))?;
            tui(&parsed).await
        }
        "tui-local" => {
            let parsed = crate::parse_args(
                &std::iter::once("tui-local".to_string())
                    .chain(argv[1..].iter().cloned())
                    .collect::<Vec<_>>(),
            )
            .1;
            validate_options(&parsed, &["config", "session"])?;
            set_effective_config(parsed.get("config").map(String::as_str))?;
            crate::tui::cmd(&parsed).await
        }
        "help" | "--help" | "-h" => {
            print!("{HELP}");
            Ok(())
        }
        // Retain all established CLI commands on the existing dispatcher. Preserve
        // an explicit config for subprocesses as well as for this process.
        _ => {
            let explicit = argv
                .windows(2)
                .find(|w| w[0] == "--config")
                .map(|w| w[1].as_str());
            set_effective_config(explicit)?;
            crate::legacy_main().await
        }
    }
}

async fn serve(flags: &BTreeMap<String, String>) -> R<()> {
    set_effective_config(flags.get("config").map(String::as_str))?;
    let cfg = Config::load(None)?;
    let data = cfg.data_path();
    std::fs::create_dir_all(&data)?;
    let token_path = token_path(flags.get("token-file"), &data);
    let token = load_token(&token_path, true)?;
    let addr = flags
        .get("listen")
        .map(String::as_str)
        .unwrap_or("127.0.0.1:7420")
        .parse::<std::net::SocketAddr>()?;
    if !addr.ip().is_loopback() {
        return Err("production surface server must bind to loopback".into());
    }
    // Адрес поверхности — в окружение процесса: дочерние подагенты (`snens
    // run --detach`) наследуют его и стримят свою работу сюда, в эту же шину.
    std::env::set_var("SNENS_SURFACE_URL", format!("http://{addr}"));
    // Токен — тоже: подагент шлёт по нему свои события в `POST …/ingest`.
    std::env::set_var("SNENS_SURFACE_TOKEN", &token);
    // API runs headless: never inherit/reuse a terminal askpass socket belonging to
    // another local TUI that shares this production data directory. The askpass
    // client fails promptly with an actionable sudo notice instead.
    std::env::set_var("SNENS_ASKPASS_DISABLED", "1");
    eprintln!(
        "[snens] HTTP/SSE {addr} · data {} · token {} · terminal askpass disabled",
        data.display(),
        token_path.display()
    );
    crate::surface_server::serve(cfg, addr, token).await
}

async fn tui(flags: &BTreeMap<String, String>) -> R<()> {
    set_effective_config(None)?;
    let cfg = Config::load(None)?;
    let token_path = token_path(flags.get("token-file"), &cfg.data_path());
    let token = load_token(&token_path, false)?;
    let endpoint = flags
        .get("endpoint")
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:7420".into());
    crate::surface_tui::run(
        endpoint,
        token,
        flags.get("session").cloned(),
        cfg.data_path().join("history.txt"),
    )
    .await
}

fn set_effective_config(explicit: Option<&str>) -> R<PathBuf> {
    let path = match explicit {
        Some(p) => PathBuf::from(p),
        None => crate::cfg::config_path(),
    };
    let path = if path.exists() {
        std::fs::canonicalize(path)?
    } else {
        path
    };
    std::env::set_var("SNENS_CONFIG", &path);
    // Every spawned run-child/review process resolves the same explicit config.
    Ok(path)
}

fn validate_options(flags: &BTreeMap<String, String>, allowed: &[&str]) -> R<()> {
    if let Some(key) = flags.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("option --{key} is not valid for this command").into());
    }
    Ok(())
}

fn token_path(flag: Option<&String>, data: &Path) -> PathBuf {
    match flag {
        Some(path) => PathBuf::from(path), // explicit path is literal, including absolute paths
        None => data.join("api.token"),
    }
}

fn load_token(path: &Path, create: bool) -> R<String> {
    if create && !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
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
    let value = std::fs::read_to_string(path)?.trim().to_owned();
    if value.len() < 32 || value.chars().any(char::is_whitespace) {
        return Err("API token must have >=32 characters and no whitespace".into());
    }
    Ok(value)
}

fn flags(args: &[String]) -> R<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let mut args = args.iter();
    while let Some(key) = args.next() {
        if !matches!(
            key.as_str(),
            "--config" | "--listen" | "--token-file" | "--endpoint" | "--session"
        ) {
            return Err(format!("unknown option {key}").into());
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {key}"))?;
        if out
            .insert(key.trim_start_matches("--").to_owned(), value.clone())
            .is_some()
        {
            return Err(format!("duplicate option {key}").into());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn token_path_is_literal_and_defaults_to_instance_data() {
        let data = Path::new("/instance");
        assert_eq!(token_path(None, data), data.join("api.token"));
        assert_eq!(
            token_path(Some(&"/tmp/key".into()), data),
            PathBuf::from("/tmp/key")
        );
    }
    #[test]
    fn cli_flags_are_strict_and_unique() {
        assert!(flags(&["--endpoint".into(), "http://localhost".into()]).is_ok());
        assert!(flags(&["--endpoint".into()]).is_err());
        assert!(flags(&["--bad".into(), "x".into()]).is_err());
        let parsed = flags(&["--endpoint".into(), "http://localhost".into()]).unwrap();
        assert!(validate_options(&parsed, &["config", "listen", "token-file"]).is_err());
        assert!(flags(&[
            "--session".into(),
            "a".into(),
            "--session".into(),
            "b".into()
        ])
        .is_err());
    }
}
