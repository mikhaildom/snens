//! Разбор аргументов: флаги и окружение в одном месте.
//!
//! Отдельный слой, потому что у бинаря ровно три вещи, которые человек задаёт
//! извне (адрес, адрес ядра, каталог клиента), и все они нужны и в `--help`, и
//! в проверках, и в юните systemd. Знание «что откуда берётся» не должно
//! расползаться по `main`.

use std::net::SocketAddr;
use std::path::PathBuf;

pub const HELP: &str = "\
snens-web — отдаёт собранный веб-клиент и проксирует /api в ядро snens.

  snens-web [--listen 127.0.0.1:5180] [--api http://127.0.0.1:7420]
            [--token-file PATH] [--dist DIR] [--no-proxy] [--allow-public]

  --listen ADDR      где слушать (по умолчанию 127.0.0.1:5180, SNENS_WEB_LISTEN)
  --api URL          адрес ядра (по умолчанию http://127.0.0.1:7420, SNENS_API_BASE)
  --token-file PATH  файл токена ядра; без него /api проксируется без Bearer
                     (по умолчанию <данные>/api.token, SNENS_API_TOKEN_FILE)
  --dist DIR         каталог собранного клиента (по умолчанию ./web рядом с
                     бинарём, SNENS_WEB_DIST)
  --no-proxy         не проксировать /api: клиент ходит в ядро сам (пейринг)
  --allow-public     разрешить не-loopback адрес в режиме прокси (по умолчанию —
                     запрещено: прокси подставляет токен ядра, и порт наружу =
                     полный доступ к API для любого)

Прокси нужен ровно затем, зачем раньше был нужен vite: браузер не должен знать
токен инстанса. С `--no-proxy` клиент получает адрес и токен сам — это режим
пейринга, как в десктопном окне.
";

#[derive(Clone, Debug)]
pub struct Args {
    pub listen: SocketAddr,
    pub api: String,
    pub token_file: Option<PathBuf>,
    pub dist: PathBuf,
    pub proxy: bool,
}

fn default_token_file() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    }?;
    let path = base.join("snens").join("api.token");
    // Файла ещё нет — это штатно (ядро создаст при старте). Возвращаем путь
    // в любом случае: прокси перечитывает файл на каждый запрос.
    Some(path)
}

impl Args {
    pub fn parse(argv: &[String]) -> Result<Option<Args>, String> {
        let mut listen = std::env::var("SNENS_WEB_LISTEN")
            .ok()
            .unwrap_or_else(|| "127.0.0.1:5180".into());
        let mut api = std::env::var("SNENS_API_BASE")
            .ok()
            .unwrap_or_else(|| "http://127.0.0.1:7420".into());
        let mut token_file = std::env::var("SNENS_API_TOKEN_FILE").ok();
        let mut dist = std::env::var("SNENS_WEB_DIST").ok();
        let mut proxy = true;
        let mut allow_public = false;

        let mut i = 0;
        while i < argv.len() {
            let arg = argv[i].as_str();
            let mut value = |name: &str| -> Result<String, String> {
                i += 1;
                argv.get(i)
                    .cloned()
                    .ok_or_else(|| format!("{name} требует значение"))
            };
            match arg {
                "-h" | "--help" => return Ok(None),
                "--listen" => listen = value("--listen")?,
                "--api" => api = value("--api")?,
                "--token-file" => token_file = Some(value("--token-file")?),
                "--dist" => dist = Some(value("--dist")?),
                "--no-proxy" => proxy = false,
                "--allow-public" => allow_public = true,
                other => return Err(format!("неизвестный аргумент: {other}")),
            }
            i += 1;
        }

        let listen: SocketAddr = listen
            .parse()
            .map_err(|e| format!("SNENS_WEB_LISTEN/--listen {listen}: {e}"))?;
        let api = api.trim_end_matches('/').to_string();
        // Валидируем один раз при старте, а не ловим на каждом запросе: голый
        // префикс пропускал `http://host with space` и вырожденный `http:`.
        if !(api.starts_with("http://") || api.starts_with("https://"))
            || reqwest::Url::parse(&api).is_err()
        {
            return Err(format!(
                "SNENS_API_BASE/--api должен быть http(s)-адресом: {api}"
            ));
        }
        // Прокси подставляет Bearer-токен ядра в каждый `/api`, а своей
        // аутентификации у snens-web нет. На не-loopback адресе это был бы
        // полный доступ к API ядра для любого, кто дотянется до порта, —
        // поэтому такой запуск требует явного подтверждения.
        if proxy && !listen.ip().is_loopback() && !allow_public {
            return Err(format!(
                "--listen {listen} слушает не только loopback, а прокси подставляет токен ядра: \
                 кто угодно, кто дотянется до порта, получит полный доступ к API. \
                 Нужно осознанно — добавь --allow-public (только за доверенным прокси/Caddy)"
            ));
        }
        // Дистрибутив по умолчанию — `web/` рядом с бинарём: ровно так его
        // кладут установщик и релизный архив. `current_exe` недоступен только
        // в вырожденном случае — тогда честно падаем на текущий каталог.
        // Пустую строку (`SNENS_WEB_DIST=""` — частый артефакт юнита/шелла)
        // считаем незаданной: иначе клиент искался бы в текущем каталоге
        // процесса.
        let dist = dist
            .filter(|p| !p.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.join("web")))
            })
            .unwrap_or_else(|| PathBuf::from("web"));
        // Токен по умолчанию — `<данные>/api.token`, как обещает `--help` и как
        // его кладёт ядро (`$XDG_DATA_HOME/snens/api.token`). Иначе прокси молча
        // ходил бы без `Authorization` и получал 401 от ядра.
        let token_file = token_file
            .filter(|p| !p.trim().is_empty())
            .map(PathBuf::from)
            .or_else(default_token_file);

        Ok(Some(Args {
            listen,
            api,
            token_file,
            dist,
            proxy,
        }))
    }
}
