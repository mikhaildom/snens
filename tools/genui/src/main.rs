// genui — генеративный UI: список библиотеки компонентов, скелет, проверка.
//
// Зачем отдельный тул: артефакт (` ```artifact ` в ответе модели) — это HTML,
// который исполняется в браузере человека. Модель не видит, что он сломан:
// незарегистрированный тег отрисуется пустотой, исключение в скрипте — тишиной.
// Проверка обязана случиться до того, как человек откроет сессию.
mod check;
mod fence;
mod library;

use std::path::PathBuf;

const USAGE: &str = "genui <list|new|check> [...]";

const HELP: &str = "\
genui — интерфейсные артефакты: библиотека компонентов, скелет, проверка.
genui list [--json]
genui new < имя-компонента > [--force]
genui check < файл.html | <session>:<seq>[:<artifact>] > [--data файл.json] [--no-browser] [--chrome путь] [--out-dir каталог] [--timeout S]

Библиотека лежит в ~/.local/share/snens/genui (SNENS_GENUI_DIR переопределяет):
lib.js, components/*.js, INDEX.md. Клиент инжектит её в каждый артефакт, поэтому
она всегда свежая: правка компонента меняет и старые артефакты.

  list        что есть: теги, файлы, отпечаток. --json — для скриптов.
  new         создать components/<имя>.js из скелета (не перезаписывает без --force)
  check       проверить артефакт: lint (чужие теги, приватная сеть, размер,
              незакрытый фенс) и рендер в браузере в ТОЙ ЖЕ песочнице, что у
              клиента. Ошибки консоли и исключения ловятся внутри iframe и
              приходят postMessage-ом; снимок кладётся в канал вложений, чтобы
              посмотреть глазами (`view`). Нет браузера — деградация до lint.

Форма `<session>:<seq>` берёт артефакт из журнала сессии: проверяется ровно то,
что увидит человек, а не копия в файле, которую модель помнит.

Коды: 0 — чисто (предупреждения допустимы), 1 — есть ошибки, 2 — неверные аргументы.";

fn die(msg: &str, code: i32) -> ! {
    eprintln!("{msg}");
    std::process::exit(code);
}

/// Чем кончился вызов: неверные аргументы (код 2) или провал по делу (код 1).
/// Справка обещает ровно эти коды, и скрипты вокруг тула на них смотрят.
enum Usage {
    Bad(String),
    Failed(String),
}

impl From<String> for Usage {
    fn from(s: String) -> Self {
        Usage::Failed(s)
    }
}

struct Args {
    cmd: String,
    rest: Vec<String>,
    json: bool,
    force: bool,
    no_browser: bool,
    data: Option<PathBuf>,
    chrome: Option<String>,
    out_dir: Option<PathBuf>,
    timeout: f64,
}

fn parse(argv: &[String]) -> Args {
    let mut a = Args {
        cmd: String::new(),
        rest: Vec::new(),
        json: false,
        force: false,
        no_browser: false,
        data: None,
        chrome: None,
        out_dir: None,
        timeout: 20.0,
    };
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        match arg {
            "--json" => a.json = true,
            "--force" => a.force = true,
            "--no-browser" => a.no_browser = true,
            "--data" | "--chrome" | "--out-dir" | "--timeout" => {
                let Some(v) = argv.get(i + 1) else {
                    die(&format!("genui: {arg} без значения\n{USAGE}"), 2);
                };
                match arg {
                    "--data" => a.data = Some(PathBuf::from(v)),
                    "--chrome" => a.chrome = Some(v.clone()),
                    "--out-dir" => a.out_dir = Some(PathBuf::from(v)),
                    _ => {
                        a.timeout = v.parse().unwrap_or_else(|_| {
                            die(&format!("genui: --timeout ждёт число, получено {v}"), 2)
                        });
                        // is_finite: `"NaN".parse::<f64>()` успешен, оба сравнения
                        // для NaN ложны, а Duration::from_secs_f64 на NaN паникует.
                        if !a.timeout.is_finite() || a.timeout <= 0.0 || a.timeout > 600.0 {
                            die(&format!("genui: --timeout вне 0..600 с: {v}"), 2);
                        }
                    }
                }
                i += 1;
            }
            _ if arg.starts_with('-') && arg != "-" => {
                die(&format!("genui: неизвестный аргумент {arg}\n{USAGE}"), 2)
            }
            _ => {
                if a.cmd.is_empty() {
                    a.cmd = arg.to_string();
                } else {
                    a.rest.push(arg.to_string());
                }
            }
        }
        i += 1;
    }
    if a.cmd.is_empty() {
        die(&format!("genui: не указана подкоманда\n{USAGE}"), 2);
    }
    a
}

// ---------------------------------------------------------------- list

fn cmd_list(a: &Args) -> Result<i32, Usage> {
    let files = library::read()?;
    let tags = library::tags(&files);
    let dir = library::dir();
    if a.json {
        let items: Vec<serde_json::Value> = tags
            .iter()
            .map(|t| {
                let file = library::file_name(t);
                let source = files
                    .iter()
                    .find(|f| f.name.ends_with(&file))
                    .map(|f| library::describe(&f.source))
                    .unwrap_or_default();
                serde_json::json!({"tag": t, "file": format!("components/{file}"), "what": source})
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "dir": dir.display().to_string(),
                "hash": library::hash(&files),
                "components": items,
            }))
            .unwrap_or_else(|_| "{}".into())
        );
        return Ok(0);
    }
    println!("библиотека {}", dir.display());
    if !library::exists(&dir) {
        println!("каталога нет — это нормально: `genui new <имя>` создаст его");
        return Ok(0);
    }
    println!(
        "файлов {}, отпечаток {}",
        files.len(),
        library::hash(&files)
    );
    if tags.is_empty() {
        println!("\nкомпонентов нет; есть только lib.js (если он есть)");
    } else {
        println!("\nкомпоненты (это всё, что можно использовать в артефакте):");
        for tag in &tags {
            let file = library::file_name(tag);
            let what = files
                .iter()
                .find(|f| f.name.ends_with(&file))
                .map(|f| library::describe(&f.source))
                .unwrap_or_default();
            if what.is_empty() {
                println!("  <{tag}>  components/{file}");
            } else {
                println!("  <{tag}>  components/{file} — {what}");
            }
        }
    }
    let index = dir.join("INDEX.md");
    if index.is_file() {
        println!("\nINDEX.md: {}", index.display());
    }
    Ok(0)
}

// ---------------------------------------------------------------- new

fn cmd_new(a: &Args) -> Result<i32, Usage> {
    let Some(name) = a.rest.first() else {
        return Err(Usage::Bad(
            "genui new: нужно имя компонента (например `quiz`)".into(),
        ));
    };
    let dir = library::dir();
    let components = dir.join("components");
    std::fs::create_dir_all(&components).map_err(|e| format!("{}: {e}", components.display()))?;
    let file = library::file_name(name);
    let path = components.join(&file);
    if path.exists() && !a.force {
        return Err(Usage::Failed(format!(
            "{} уже есть — правь его или добавь --force",
            path.display()
        )));
    }
    std::fs::write(&path, library::skeleton(name))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    // lib.js — точка входа библиотеки: пустой файл лучше его отсутствия, иначе
    // первый же компонент положит общие утилиты некуда.
    let lib = dir.join("lib.js");
    if !lib.exists() {
        std::fs::write(
            &lib,
            "// lib.js — общие утилиты библиотеки genui. Инжектится в каждый артефакт\n\
             // ДО компонентов: здесь только то, что нужно нескольким компонентам.\n",
        )
        .map_err(|e| format!("{}: {e}", lib.display()))?;
    }
    // INDEX.md — то, что читает скилл и `genui list`: регенерируем целиком из
    // файлов, чтобы он не разошёлся с содержимым каталога.
    let _ = write_index(&dir);
    // Тег — того компонента, который только что создан, а не первый в
    // библиотеке: подсказка обязана называть свой файл.
    let tag = format!("my-{}", file.trim_end_matches(".js"));
    println!("{} — скелет <{tag}>", path.display());
    println!("дальше: заполнить, затем `genui check` на артефакте, который его использует");
    println!("библиотека инжектится в артефакты сразу: отдельная регистрация не нужна");
    Ok(0)
}

/// Переписать `INDEX.md` по фактическому содержимому библиотеки.
///
/// Генерируется, а не правится руками: файл — производная от `components/`, и
/// разойтись с ним не должен. Ошибка записи не валит `new`: компонент важнее.
fn write_index(dir: &std::path::Path) -> Result<(), String> {
    let files = library::read()?;
    let tags = library::tags(&files);
    let mut text = String::from(concat!(
        "# Библиотека компонентов genui\n",
        "\n",
        "Инжектится в каждый артефакт. Это всё, что можно использовать в разметке ",
        "без собственного кода. Файл генерируется: правь `components/`, а не его.\n",
        "\n",
    ));
    if tags.is_empty() {
        text.push_str("Компонентов пока нет: `genui new <имя>` создаст первый.\n");
    } else {
        text.push_str("| тег | файл | что |\n|---|---|---|\n");
        for tag in &tags {
            let file = library::file_name(tag);
            let what = files
                .iter()
                .find(|f| f.name.ends_with(&file))
                .map(|f| library::describe(&f.source))
                .unwrap_or_default();
            text.push_str(&format!("| `<{tag}>` | `components/{file}` | {what} |\n"));
        }
    }
    std::fs::write(dir.join("INDEX.md"), text).map_err(|e| format!("INDEX.md: {e}"))
}

// ---------------------------------------------------------------- check

/// Вынуть артефакт из журнала сессии: `<session>:<seq>[:<artifact>]`.
fn from_session(cfg: &str) -> Result<(fence::Artifact, String), String> {
    let mut parts = cfg.split(':');
    let session = parts.next().unwrap_or("");
    let seq: u64 = parts
        .next()
        .ok_or("форма: <session>:<seq>[:<artifact>]")?
        .parse()
        .map_err(|_| "seq должен быть числом (номер строки журнала)".to_string())?;
    let want = parts.next().unwrap_or("");
    if session.is_empty() {
        return Err("пустое имя сессии".into());
    }
    // Имя сессии приходит из командной строки: `../..` и абсолютный путь увели бы
    // чтение за пределы каталога сессий.
    if session.contains(['/', '\\'])
        || session.contains("..")
        || std::path::Path::new(session).is_absolute()
    {
        return Err(format!("недопустимое имя сессии `{session}`"));
    }
    if seq == 0 {
        return Err("seq нумеруется с 1: 0 — не строка журнала".into());
    }
    let dir = sessions_dir();
    let path = dir.join(format!("{session}.jsonl"));
    if !path.is_file() {
        return Err(format!("нет журнала {}", path.display()));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let line = text
        .lines()
        .nth(seq.saturating_sub(1) as usize)
        .ok_or_else(|| format!("в журнале нет строки {seq}"))?;
    let rec: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("строка {seq} не JSON: {e}"))?;
    if rec.get("kind").and_then(|v| v.as_str()) != Some("assistant") {
        return Err(format!(
            "строка {seq} — не ответ модели (kind: {})",
            rec.get("kind").and_then(|v| v.as_str()).unwrap_or("?")
        ));
    }
    let body = rec
        .get("text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("в строке {seq} нет текста"))?;
    let arts = fence::parse(body);
    if arts.is_empty() {
        return Err(format!("в строке {seq} нет фенса artifact"));
    }
    let art = if want.is_empty() {
        arts.into_iter().next().unwrap()
    } else {
        arts.into_iter()
            .find(|a| a.head.id == want)
            .ok_or_else(|| format!("артефакта `{want}` в строке {seq} нет"))?
    };
    Ok((art, format!("{session}:{seq}")))
}

fn sessions_dir() -> PathBuf {
    if let Ok(d) = std::env::var("SNENS_SESSIONS_DIR") {
        if !d.trim().is_empty() {
            return PathBuf::from(d);
        }
    }
    let base = std::env::var("SNENS_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".local/share/snens")
        });
    base.join("sessions")
}

fn cmd_check(a: &Args) -> Result<i32, Usage> {
    let Some(target) = a.rest.first() else {
        return Err(Usage::Bad(
            "genui check: нужен файл или <session>:<seq>".into(),
        ));
    };
    let files = library::read()?;
    let tags = library::tags(&files);

    let (art, origin) = if target.contains(':') && !target.ends_with(".html") {
        let (art, origin) = from_session(target)?;
        (art, origin)
    } else {
        let path = PathBuf::from(target);
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        // Файл может быть и артефактом целиком, и HTML без фенса.
        let mut arts = fence::parse(&text);
        let art = if arts.is_empty() {
            fence::Artifact {
                head: fence::Head::default(),
                body: text,
                open: false,
                index: 1,
            }
        } else {
            arts.remove(0)
        };
        (art, path.display().to_string())
    };

    let mut report = check::lint(&art, &tags);
    println!(
        "артефакт {} (id: {}, {} байт) из {}",
        art.index,
        if art.head.id.is_empty() {
            "—"
        } else {
            &art.head.id
        },
        art.body.len(),
        origin
    );

    let mut degraded = false;
    if !a.no_browser {
        // --data объявлен в справке и схеме: молча его игнорировать — врать о
        // контракте. Читаем и разбираем здесь, ошибку показываем как есть.
        let data = match &a.data {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .map_err(|e| format!("--data {}: {e}", p.display()))?;
                Some(
                    serde_json::from_str::<serde_json::Value>(&text)
                        .map_err(|e| format!("--data {}: не JSON: {e}", p.display()))?,
                )
            }
            None => None,
        };
        match check::check_in_browser(
            &art,
            &files,
            data.as_ref(),
            a.chrome.as_deref(),
            a.out_dir.as_deref(),
            a.timeout,
        ) {
            Ok(browser) => {
                for f in browser.findings {
                    report.findings.push(f);
                }
                if let Some(info) = &browser.rendered {
                    println!(
                        "рендер: {} симв. текста, {} элементов, высота {}{}",
                        info.get("text").and_then(|v| v.as_u64()).unwrap_or(0),
                        info.get("elements").and_then(|v| v.as_u64()).unwrap_or(0),
                        info.get("scrollHeight")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0),
                        match info
                            .get("brokenImages")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0)
                        {
                            0 => String::new(),
                            n => format!(", битых картинок {n}"),
                        }
                    );
                    if info.get("text").and_then(|v| v.as_u64()).unwrap_or(0) == 0
                        && info.get("elements").and_then(|v| v.as_u64()).unwrap_or(0) <= 3
                    {
                        report.warn("документ пустой: текста нет и элементов почти нет");
                    }
                }
            }
            Err(e) => {
                // Деградация вместо падения: lint уже дал полезный ответ, а
                // браузера на машине может не быть (десктоп-поставка).
                degraded = true;
                report.warn(format!("браузер не проверен: {e}"));
            }
        }
    } else {
        degraded = true;
    }

    let errors = report.errors();
    for f in &report.findings {
        let mark = match f.level {
            check::Level::Error => "ошибка",
            check::Level::Warn => "внимание",
        };
        println!("{mark}: {}", f.text);
    }
    if degraded {
        println!("проверена только разметка (без браузера): рендер и скрипты — нет");
    }
    if errors == 0 {
        println!("\nчисто");
        Ok(0)
    } else {
        println!("\nошибок {errors}");
        Ok(1)
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv.iter().any(|a| a == "-h" || a == "--help") {
        println!("{HELP}");
        return;
    }
    let a = parse(&argv);
    let result = match a.cmd.as_str() {
        "list" => cmd_list(&a),
        "new" => cmd_new(&a),
        "check" => cmd_check(&a),
        other => Err(Usage::Bad(format!(
            "genui: неизвестная подкоманда `{other}`\n{USAGE}"
        ))),
    };
    match result {
        Ok(code) => std::process::exit(code),
        // Ошибка разбора аргументов — код 2, провал проверки — 1. `Usage` отличает
        // их от «артефакт не прошёл»: скрипты вокруг тула на это смотрят.
        Err(Usage::Bad(msg)) => {
            eprintln!("genui: {msg}");
            std::process::exit(2)
        }
        Err(Usage::Failed(msg)) => {
            eprintln!("genui: {msg}");
            std::process::exit(1)
        }
    }
}
