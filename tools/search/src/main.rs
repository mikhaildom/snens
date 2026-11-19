// search — веб-поиск через локальный SearXNG. Тул, а не часть ядра: ядру смотреть
// в интернет нечем и незачем, модель зовёт этот бинарник как обычный процесс.
//
// Адрес инстанса: SEARXNG_URL → ~/.config/snens/search.toml (url) → 127.0.0.1:8888.
use serde::Deserialize;
use serde_json::{json, Value};

const HELP: &str = "\
search — веб-поиск через SearXNG: заголовок, ссылка и сниппет на каждый результат.
search <запрос> [--n N] [--page P] [--lang L] [--fresh day|week|month|year] [--category general|news|it] [--site домен] [--json]

По умолчанию 10 результатов (максимум 30), вывод человекочитаемый: три строки
на результат и футер со счётчиками. --json — нормализованный JSON одной строкой.
Адрес: SEARXNG_URL или url в ~/.config/snens/search.toml (дефолт http://127.0.0.1:8888).";

const USAGE: &str =
    "search <запрос> [--n N] [--page P] [--lang L] [--fresh day|week|month|year] [--category general|news|it] [--site домен] [--json]";

const SNIPPET_MAX: usize = 300;
const N_MAX: usize = 30;

fn die(code: i32, msg: String) -> ! {
    eprintln!("{msg}");
    std::process::exit(code);
}

struct Args {
    query: String,
    n: usize,
    page: u32,
    lang: String,
    fresh: Option<String>,
    category: String,
    site: Option<String>,
    json: bool,
}

/// `--n=5` → `["--n", "5"]`: один цикл разбора вместо двух (как в read).
fn expand(argv: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for a in argv {
        match a.strip_prefix("--").and_then(|r| r.split_once('=')) {
            Some((k, v)) => {
                out.push(format!("--{k}"));
                out.push(v.to_string());
            }
            None => out.push(a.clone()),
        }
    }
    out
}

const FRESH: [&str; 4] = ["day", "week", "month", "year"];
const CATEGORIES: [&str; 3] = ["general", "news", "it"];

fn parse(argv: &[String]) -> Args {
    let argv = expand(argv);
    let mut query: Option<String> = None;
    let (mut n, mut page) = (10usize, 1u32);
    let mut lang = "all".to_string();
    let (mut fresh, mut site) = (None, None);
    let mut category = "general".to_string();
    let mut json = false;
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        let value = |what: &str| -> String {
            match argv.get(i + 1) {
                // Следующий флаг — не значение: `--lang --json` иначе съедал --json.
                Some(v) if !v.starts_with("--") => v.clone(),
                _ => die(2, format!("search: {what} без значения\n{USAGE}")),
            }
        };
        match a {
            "--n" => {
                let v = value("--n");
                n = v
                    .parse::<usize>()
                    .unwrap_or_else(|_| {
                        die(2, format!("search: --n ждёт число, получено {v}\n{USAGE}"))
                    })
                    .clamp(1, N_MAX);
                i += 2;
            }
            "--page" => {
                let v = value("--page");
                page = v
                    .parse::<u32>()
                    .unwrap_or_else(|_| {
                        die(
                            2,
                            format!("search: --page ждёт число, получено {v}\n{USAGE}"),
                        )
                    })
                    .max(1);
                i += 2;
            }
            "--lang" => {
                lang = value("--lang");
                i += 2;
            }
            "--fresh" => {
                let v = value("--fresh");
                if !FRESH.contains(&v.as_str()) {
                    die(
                        2,
                        format!("search: --fresh ждёт day|week|month|year, получено {v}\n{USAGE}"),
                    );
                }
                fresh = Some(v);
                i += 2;
            }
            "--category" => {
                let v = value("--category");
                if !CATEGORIES.contains(&v.as_str()) {
                    die(
                        2,
                        format!("search: --category ждёт general|news|it, получено {v}\n{USAGE}"),
                    );
                }
                category = v;
                i += 2;
            }
            "--site" => {
                site = Some(value("--site"));
                i += 2;
            }
            "--json" => {
                json = true;
                i += 1;
            }
            _ if a.starts_with('-') && a != "-" => {
                die(2, format!("search: неизвестный аргумент {a}\n{USAGE}"))
            }
            _ => {
                if query.is_some() {
                    die(2, format!("search: запрос указан дважды\n{USAGE}"));
                }
                query = Some(a.to_string());
                i += 1;
            }
        }
    }
    let query = query.unwrap_or_else(|| die(2, format!("search: не указан запрос\n{USAGE}")));
    Args {
        query,
        n,
        page,
        lang,
        fresh,
        category,
        site,
        json,
    }
}

#[derive(Deserialize, Default)]
struct File {
    url: Option<String>,
}

/// env → файл → дефолт. Раскладка та же, что у конфига харнесса и memory.
fn base_url() -> String {
    // Значение из env нормализуем: пробелы по краям давали URL с пробелом внутри,
    // а reqwest такой отвергает с непонятной ошибкой.
    let env = std::env::var("SEARXNG_URL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let file = read_file();
    // Пустой `url = ""` в конфиге — не адрес: иначе запрос шёл бы на `/search`.
    let pick = env
        .or(file.url.map(|u| u.trim().to_string()))
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8888".into());
    pick.trim_end_matches('/').to_string()
}

/// Разбор `search.toml`. Ошибку не глотаем: опечатка в конфиге иначе неотличима
/// от отсутствия файла, и запрос молча уходил бы на дефолтный инстанс.
fn read_file() -> File {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return File::default(),
        Err(e) => {
            eprintln!("search: {}: {e} — беру адрес по умолчанию", path.display());
            return File::default();
        }
    };
    match toml::from_str(&text) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "search: {} не разобран: {e} — беру адрес по умолчанию",
                path.display()
            );
            File::default()
        }
    }
}

fn config_path() -> std::path::PathBuf {
    let home = ["HOME", "USERPROFILE"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| ".".into());
    let appdata = std::env::var("APPDATA").ok().filter(|v| !v.is_empty());
    let dir = match (cfg!(windows), appdata) {
        (true, Some(a)) => format!("{a}/snens"),
        _ => format!("{home}/.config/snens"),
    };
    std::path::PathBuf::from(format!("{dir}/search.toml"))
}

/// Обрезка сниппета по символам, а не байтам — иначе рвётся внутри UTF-8.
/// Управляющие символы и ESC-последовательности выкидываем: заголовок страницы
/// приходит из интернета и может перекрасить терминал или подделать строку вывода.
fn clip(s: &str, max: usize) -> String {
    let clean: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let s = clean.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Поиск в ответе SearXNG: поле results[], у каждого свой набор полей.
#[derive(Deserialize, Default, Clone)]
struct Hit {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    engine: String,
    #[serde(default, rename = "publishedDate")]
    published: Option<String>,
}

#[derive(Deserialize, Default)]
struct Answer {
    #[serde(default)]
    results: Vec<Hit>,
    // Движки, которые отвалились (429/CAPTCHA/timeout). Без этого поля «ничего
    // не найдено» не отличить от «движки как раз упали» — а они падают регулярно.
    // Форма поля в разных версиях SearXNG разная: пары [engine, reason] или просто
    // имена. Строгий тип валил весь разбор и превращал рабочий ответ в «не JSON».
    #[serde(default)]
    unresponsive_engines: Vec<Value>,
}

type SearchResults = (usize, Vec<Value>, Vec<String>);

fn fetch(base: &str, a: &Args) -> Result<SearchResults, String> {
    let mut q = a.query.clone();
    if let Some(site) = &a.site {
        q.push_str(&format!(" site:{site}"));
    }
    let mut query: Vec<(&str, String)> = vec![
        ("q", q),
        ("format", "json".into()),
        ("language", a.lang.clone()),
        ("categories", a.category.clone()),
        ("pageno", a.page.to_string()),
        ("safesearch", "0".into()),
    ];
    if let Some(f) = &a.fresh {
        query.push(("time_range", f.clone()));
    }
    let url = format!("{base}/search");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("search: не собрать HTTP-клиент: {e}"))?;
    let resp = client
        .get(&url)
        .query(&query)
        .send()
        .map_err(|e| format!("search: SearXNG недоступен ({url}): {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!(
            "search: SearXNG ответил HTTP {} ({url})",
            status.as_u16()
        ));
    }
    let body = resp
        .text()
        .map_err(|e| format!("search: не прочитать ответ ({url}): {e}"))?;
    let ans: Answer = serde_json::from_str(&body)
        .map_err(|e| format!("search: SearXNG вернул не JSON ({url}): {e}"))?;
    let total = ans.results.len();
    let results = ans
        .results
        .into_iter()
        .take(a.n)
        .map(|h| {
            json!({
                "title": h.title,
                "url": h.url,
                "snippet": clip(&h.content, SNIPPET_MAX),
                "engine": h.engine,
                "published": h.published,
            })
        })
        .collect();
    // Приводим обе формы к строке «engine (reason)».
    let dead: Vec<String> = ans
        .unresponsive_engines
        .iter()
        .map(|v| match v {
            Value::Array(a) => {
                let name = a.first().and_then(Value::as_str).unwrap_or("?");
                let why = a
                    .get(1)
                    .and_then(Value::as_str)
                    .unwrap_or("причина не сказана");
                format!("{name} ({why})")
            }
            other => other.as_str().unwrap_or("?").to_string(),
        })
        .collect();
    Ok((total, results, dead))
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "-h" || argv[0] == "--help" {
        if argv.is_empty() {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
        println!("{HELP}");
        std::process::exit(0);
    }
    let a = parse(&argv);
    let base = base_url();
    let (total, results, unresponsive) = match fetch(&base, &a) {
        Ok(v) => v,
        Err(e) => die(1, e),
    };
    let dead: Vec<String> = unresponsive;

    if a.json {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "query": a.query,
                "results": results,
                "unresponsive_engines": dead,
            }))
            .unwrap_or_else(|e| die(1, format!("search: {e}")))
        );
        return;
    }

    if results.is_empty() {
        // Различие, ради которого поле и читается: пусто из-за упавших движков —
        // это не «в интернете нет», а «инстанс сейчас не ищет».
        if dead.is_empty() {
            println!("ничего не найдено");
        } else {
            println!("ничего не найдено; отвалились движки: {}", dead.join(", "));
        }
        return;
    }
    let mut engines = std::collections::BTreeSet::new();
    for (i, r) in results.iter().enumerate() {
        let s = |k: &str| r[k].as_str().unwrap_or_default();
        println!("{}. {}", i + 1, s("title").trim());
        println!("   {}", s("url").trim());
        let snippet = s("snippet").trim();
        if !snippet.is_empty() {
            println!("   {snippet}");
        }
        engines.insert(s("engine").trim().to_string());
    }
    println!(
        "найдено {total}, показано {}, движков {}",
        results.len(),
        engines.len()
    );
    if !dead.is_empty() {
        println!("отвалились движки: {}", dead.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_cuts_by_chars_not_bytes() {
        assert_eq!(clip("  короткий  ", SNIPPET_MAX), "короткий");
        let long = "я".repeat(500);
        let out = clip(&long, 300);
        assert_eq!(out.chars().count(), 301); // 300 символов + многоточие
        assert!(out.ends_with('…'));
        // ровно на границе — без усечения
        let exact = "x".repeat(300);
        assert_eq!(clip(&exact, 300), exact);
    }

    #[test]
    fn args_defaults_and_flags() {
        let v: Vec<String> = ["rust", "--n", "5", "--json"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let a = parse(&v);
        assert_eq!(a.query, "rust");
        assert_eq!(a.n, 5);
        assert_eq!(a.page, 1);
        assert_eq!(a.lang, "all");
        assert_eq!(a.category, "general");
        assert!(a.json);
        assert!(a.fresh.is_none());

        let v: Vec<String> = ["q", "--n=100", "--fresh=week", "--site=example.com"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let a = parse(&v);
        assert_eq!(a.n, 30); // максимум
        assert_eq!(a.fresh.as_deref(), Some("week"));
        assert_eq!(a.site.as_deref(), Some("example.com"));
    }

    #[test]
    fn parses_searxng_answer() {
        let raw = r#"{"query":"x","results":[
            {"title":"T","url":"http://a","content":"c","engine":"ddg","publishedDate":"2024-01-01"},
            {"title":"U","url":"http://b","engine":"google"}
        ]}"#;
        let ans: Answer = serde_json::from_str(raw).unwrap();
        assert_eq!(ans.results.len(), 2);
        assert_eq!(ans.results[0].published.as_deref(), Some("2024-01-01"));
        assert_eq!(ans.results[1].content, "");
        assert!(ans.results[1].published.is_none());
    }

    #[test]
    fn reads_unresponsive_engines() {
        let raw = r#"{"query":"x","results":[],
            "unresponsive_engines":[["brave","too many requests"],["duckduckgo","timeout"]]}"#;
        let ans: Answer = serde_json::from_str(raw).unwrap();
        assert!(ans.results.is_empty());
        assert_eq!(ans.unresponsive_engines.len(), 2);
        assert_eq!(ans.unresponsive_engines[0][0].as_str(), Some("brave"));
    }

    #[test]
    fn flat_unresponsive_engines_do_not_break_the_parse() {
        // Старые версии отдают просто имена: раньше это был «не JSON» и пустой ответ.
        let raw = r#"{"query":"x","results":[{"title":"t","url":"u","content":"c"}],
            "unresponsive_engines":["brave"]}"#;
        let ans: Answer = serde_json::from_str(raw).unwrap();
        assert_eq!(ans.results.len(), 1);
        assert_eq!(ans.unresponsive_engines.len(), 1);
    }
}
