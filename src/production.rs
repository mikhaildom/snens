use crate::{cfg::Config, R};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

const HELP: &str = "snens — агент с инструментами\n\
  serve [--config PATH] [--listen 127.0.0.1:7420] [--token-file PATH] [--data-dir DIR]\n\
        [--allow-remote] [--insecure-public] — слушать не только loopback (см. docs/pairing.md)\n\
  tui [--config PATH] [--endpoint URL] [--token-file PATH] [--session ID] — диалог с ядром\n\
  tui-local [--session ID] — прежний локальный TUI, без ядра (совместимость)\n\
  run, review, tools, build, config, model и остальные — ядро в терминале (snens help)\n\n\
Конфиг: ~/.config/snens/config.toml; токен API: <каталог данных>/api.token.\n\
Сервер слушает только loopback. sudo в этом TUI не спрашивается: секреты живут локально.\n";

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
            let parsed = flags_for(&argv[1..], &SERVE_SPEC)?;
            serve(&parsed).await
        }
        "tui" => {
            let parsed = flags_for(&argv[1..], &TUI_SPEC)?;
            set_effective_config(parsed.get("config").map(String::as_str))?;
            tui(&parsed).await
        }
        // `tui-local` — прежний локальный TUI: своя спека, а не белый список
        // руками. Раньше правило «что допустимо» здесь было четвёртой копией и
        // уже расходилось с `TUI_SPEC`.
        "tui-local" => {
            let parsed = flags_for(&argv[1..], &TUI_LOCAL_SPEC)?;
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
            // `--config` для legacy-подкоманд раньше искался `windows(2)`: если
            // флаг стоял последним или был один, пара не находилась и конфиг
            // молча игнорировался. Берём его тем же парсером, что и остальное.
            let explicit = parse_flags(&argv[1..], &CLI_SPEC)
                .ok()
                .and_then(|(parsed, _)| parsed.get("config").cloned());
            set_effective_config(explicit.as_deref())?;
            crate::legacy_main().await
        }
    }
}

async fn serve(flags: &BTreeMap<String, String>) -> R<()> {
    set_effective_config(flags.get("config").map(String::as_str))?;
    // Отдельный инстанс — это отдельный каталог данных, а не вторая сборка:
    // стенд/эксперимент получает свои сессии, ключ и токен, не задевая рабочие.
    if let Some(dir) = flags.get("data-dir") {
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir)?;
        let dir = std::fs::canonicalize(dir)?;
        std::env::set_var("SNENS_DATA_DIR", &dir);
    }
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
    // Не-loopback разрешён только явно и только в приватной сети: API защищён
    // одним bearer-токеном и без TLS, поэтому «открыть наружу» — решение
    // человека, а не следствие опечатки в адресе (docs/pairing.md).
    let remote = !addr.ip().is_loopback();
    if remote {
        if !flags.contains_key("allow-remote") {
            // Про причину «не приватный» здесь молчим: это отдельная проверка
            // ниже, а склеенное сообщение путало («100.x не приватный»).
            return Err(format!(
                "ядро слушает только loopback (сейчас --listen {addr}). Для доступа \
                 с другого устройства добавь --allow-remote (в приватной сети: \
                 tailnet/WireGuard/LAN) — см. docs/pairing.md"
            )
            .into());
        }
        if !crate::remote::allowed_without_override(addr.ip()) && !flags.contains_key("insecure-public") {
            return Err(crate::remote::refusal(addr.ip()).into());
        }
        eprintln!(
            "[snens] ВНИМАНИЕ: слушаю {addr} — API доступен по сети с одним bearer-токеном. \
             Снаружи пускай только приватную сеть (tailnet/WireGuard) или TLS."
        );
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
    crate::surface_server::serve_with_ready(cfg, addr, token, &token_path).await
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

/// Единственный разбор флагов на всё ядро.
///
/// Раньше парсеров было три: этот, `cli::parse_args` (свой список «что берёт
/// значение», своё поведение на дублях и на неизвестных флагах) и таблица
/// разрешённых опций в `validate_options`. Списки расходились: новый флаг,
/// добавленный в одном месте, молча получал `"1"` в другом. Теперь правило одно,
/// а различия — в `FlagSpec`.
pub struct FlagSpec {
    /// Флаги, которые носят значение (`--config PATH`).
    pub values: &'static [&'static str],
    /// Флаги-переключатели (`--allow-remote`): значения не носят.
    pub switches: &'static [&'static str],
    /// Строгий режим — для команд ядра: неизвестный флаг и повтор — ошибка.
    /// Мягкий — для подкоманд: они принимают свои переключатели (`--detach`,
    /// `--new`), а повтор просто перебивает прежнее значение.
    pub strict: bool,
}

pub fn parse_flags(args: &[String], spec: &FlagSpec) -> R<(BTreeMap<String, String>, Vec<String>)> {
    let mut flags = BTreeMap::new();
    let mut positional = Vec::new();
    let mut it = args.iter();
    while let Some(key) = it.next() {
        if !key.starts_with("--") {
            positional.push(key.clone());
            continue;
        }
        let takes_value = spec.values.contains(&key.as_str());
        if spec.strict && !takes_value && !spec.switches.contains(&key.as_str()) {
            return Err(format!("unknown option {key}").into());
        }
        let value = if takes_value {
            it.next()
                .ok_or_else(|| format!("missing value for {key}"))?
                .clone()
        } else {
            "1".to_string()
        };
        let previous = flags.insert(key.trim_start_matches("--").to_owned(), value);
        if spec.strict && previous.is_some() {
            return Err(format!("duplicate option {key}").into());
        }
    }
    Ok((flags, positional))
}

/// Что принимает `serve` (правило было в `validate_options`). Перечни у команд
/// разные намеренно: `serve --endpoint` — опечатка, а не настройка, и раньше она
/// ловилась именно здесь.
const SERVE_SPEC: FlagSpec = FlagSpec {
    values: &[
        "--config",
        "--listen",
        "--token-file",
        "--data-dir",
        "--tls-cert",
        "--tls-key",
    ],
    switches: &["--allow-remote", "--insecure-public"],
    strict: true,
};

/// Что принимает `tui`.
const TUI_SPEC: FlagSpec = FlagSpec {
    values: &["--config", "--endpoint", "--token-file", "--session"],
    switches: &[],
    strict: true,
};

/// Что принимает `tui-local` (прежний локальный TUI).
const TUI_LOCAL_SPEC: FlagSpec = FlagSpec {
    values: &["--config", "--session"],
    switches: &[],
    strict: true,
};

/// Мягкая спека подкоманд: они принимают свои переключатели (`--detach`,
/// `--new`, `--json`), а повтор флага перебивает прежнее значение.
pub const CLI_SPEC: FlagSpec = FlagSpec {
    values: &[
        "--session",
        "--config",
        "--parent",
        "--job",
        "--older-than",
        "--repo",
        "--endpoint",
        "--token-file",
    ],
    switches: &[],
    strict: false,
};

fn flags_for(args: &[String], spec: &FlagSpec) -> R<BTreeMap<String, String>> {
    Ok(parse_flags(args, spec)?.0)
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
        // Перечни у команд разные: `--endpoint` — настройка `tui`, а не `serve`.
        let serve = |a: &[String]| flags_for(a, &SERVE_SPEC);
        let tui = |a: &[String]| flags_for(a, &TUI_SPEC);
        assert!(tui(&["--endpoint".into(), "http://localhost".into()]).is_ok());
        assert!(serve(&["--endpoint".into(), "http://localhost".into()]).is_err());
        // Флаг со значением не может остаться без него.
        assert!(serve(&["--config".into()]).is_err());
        // Неизвестный флаг и повтор — ошибка строгого режима.
        assert!(serve(&["--bad".into(), "x".into()]).is_err());
        assert!(serve(&["--config".into(), "a".into(), "--config".into(), "b".into()]).is_err());
        // Переключатель значения не носит: `--allow-remote` — это «включено».
        let parsed = tui(&["--endpoint".into(), "http://localhost".into()]).unwrap();
        assert_eq!(parsed["endpoint"], "http://localhost");
        let parsed = serve(&["--allow-remote".into()]).unwrap();
        assert_eq!(parsed["allow-remote"], "1");
    }

}
