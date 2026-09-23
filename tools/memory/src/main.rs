// Долгая память агента: Hindsight по REST. Тул, а не часть ядра — ядро знает
// только argv-контракт (`memory recall <текст>` / `memory retain --context …`),
// модель зовёт то же самое своими руками.
//
// Банк общий с Гермесом: пишем со своим тегом, читаем всё, что там есть.
use std::io::Read;

use serde::Deserialize;
use serde_json::{json, Value};

const USAGE: &str =
    "memory recall <запрос> | memory retain [--context C] [--tag T] [текст | -] | memory stats";

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "-h" || argv[0] == "--help" {
        help();
        return;
    }
    let cfg = Cfg::load();
    let code = match argv[0].as_str() {
        "recall" => recall(&cfg, &argv[1..]),
        "retain" => retain(&cfg, &argv[1..]),
        "stats" => stats(&cfg),
        other => {
            eprintln!("memory: неизвестная команда {other}");
            eprintln!("{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

fn help() {
    println!("memory — долгая память агента (Hindsight): поиск фактов и запись новых");
    println!("{USAGE}");
    println!(
        "\nrecall  — семантический поиск по общему банку, по строке на факт.\n\
         retain  — записать факт. Текст берётся из аргументов, иначе со stdin;\n\
         \x20         JSON хода {{\"user\":…,\"assistant\":…}} превращается в разговор.\n\
         \x20         Запись асинхронная: печатается id операции.\n\
         stats   — что в банке: узлы, документы, зависшие операции.\n\
         \nНастройки: ~/.config/snens/memory.toml (url, bank, budget, tag), поверх —\n\
         переменные SNENS_MEMORY_URL / SNENS_MEMORY_BANK. Нет сервера — код 1."
    );
}

struct Cfg {
    url: String,
    bank: String,
    budget: String,
    tags: Vec<String>,
}

#[derive(Deserialize, Default)]
struct File {
    url: Option<String>,
    bank: Option<String>,
    budget: Option<String>,
    tag: Option<Vec<String>>,
}

impl Cfg {
    /// env → файл → дефолт. Путь к файлу — та же раскладка, что у конфига харнесса.
    fn load() -> Cfg {
        let f: File = std::fs::read_to_string(config_path())
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default();
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Cfg {
            url: env("SNENS_MEMORY_URL")
                .or(f.url)
                .unwrap_or_else(|| "http://127.0.0.1:9178".into()),
            bank: env("SNENS_MEMORY_BANK")
                .or(f.bank)
                .unwrap_or_else(|| "rp-bot-default".into()),
            budget: f.budget.unwrap_or_else(|| "mid".into()),
            tags: f.tag.unwrap_or_else(|| vec!["agent:snens".into()]),
        }
    }

    fn call(&self, method: &str, action: &str, body: Option<Value>) -> Result<Value, String> {
        let url = format!(
            "{}/v1/default/banks/{}/{action}",
            self.url.trim_end_matches('/'),
            self.bank
        );
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .build()
            .map_err(|e| e.to_string())?;
        let req = match method {
            "GET" => client.get(&url),
            _ => client.post(&url).json(&body.unwrap_or(Value::Null)),
        };
        let resp = req
            .send()
            .map_err(|e| format!("Hindsight недоступен ({url}): {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("Hindsight {action}: {}", resp.status()));
        }
        resp.json().map_err(|e| format!("Hindsight {action}: {e}"))
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
    std::path::PathBuf::from(format!("{dir}/memory.toml"))
}

fn recall(cfg: &Cfg, args: &[String]) -> i32 {
    let query = args.join(" ");
    if query.trim().is_empty() {
        eprintln!("memory recall: нужен запрос");
        return 2;
    }
    let body = json!({"query": query, "budget": cfg.budget, "max_tokens": 4096});
    match cfg.call("POST", "memories/recall", Some(body)) {
        Err(e) => {
            eprintln!("memory: {e}");
            1
        }
        // Пустой вывод = в банке ничего по теме: хук тогда ничего не впрыскивает.
        Ok(v) => {
            for (i, hit) in v["results"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .enumerate()
            {
                let text = hit["text"].as_str().unwrap_or_default().trim();
                if !text.is_empty() {
                    println!("{}. {text}", i + 1);
                }
            }
            0
        }
    }
}

/// Аргументы retain: контекст, теги, текст. Чистая функция — на ней проверка.
fn parse_retain(args: &[String], base_tags: &[String]) -> (String, Vec<String>, Vec<String>) {
    let (mut context, mut tags, mut text) = (String::new(), base_tags.to_vec(), Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let val = |it: &mut std::slice::Iter<String>| it.next().cloned().unwrap_or_default();
        if a == "--context" || a == "-c" {
            context = val(&mut it);
        } else if a == "--tag" || a == "-t" {
            tags.push(val(&mut it));
        } else if let Some(v) = a.strip_prefix("--context=") {
            context = v.to_string();
        } else if let Some(v) = a.strip_prefix("--tag=") {
            tags.push(v.to_string());
        } else if a != "-" {
            text.push(a.clone());
        }
    }
    (context, tags, text)
}

/// JSON хода от ядра → текст разговора. Что писать в память — решает провайдер,
/// а не харнесс: ядру форматировать нечем и незачем.
fn turn_text(raw: &str) -> Option<String> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let field = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let (user, assistant) = (field("user"), field("assistant"));
    if user.is_empty() && assistant.is_empty() {
        return None;
    }
    let mut s = String::new();
    if !user.is_empty() {
        s.push_str(&format!("User: {user}"));
    }
    if !assistant.is_empty() {
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(&format!("Assistant: {assistant}"));
    }
    Some(s)
}

fn retain(cfg: &Cfg, args: &[String]) -> i32 {
    let (context, tags, argv_text) = parse_retain(args, &cfg.tags);
    let raw = if argv_text.is_empty() {
        let mut s = String::new();
        let _ = std::io::stdin().read_to_string(&mut s);
        s
    } else {
        argv_text.join(" ")
    };
    let content = turn_text(&raw).unwrap_or(raw);
    if content.trim().is_empty() {
        eprintln!("memory retain: нет текста — ни в аргументах, ни на stdin");
        return 2;
    }
    let item = json!({"content": content.trim(), "context": context, "tags": tags});
    // Асинхронно: извлечение фактов — это LLM и десятки секунд, тул столько не ждёт.
    let body = json!({"items": [item], "async": true});
    match cfg.call("POST", "memories", Some(body)) {
        Err(e) => {
            eprintln!("memory: {e}");
            1
        }
        Ok(v) => {
            let op = v["operation_id"].as_str().unwrap_or("?");
            println!("op {op}");
            0
        }
    }
}

fn stats(cfg: &Cfg) -> i32 {
    match cfg.call("GET", "stats", None) {
        Err(e) => {
            eprintln!("memory: {e}");
            1
        }
        Ok(v) => {
            println!("банк: {}", v["bank_id"].as_str().unwrap_or("?"));
            println!(
                "узлов: {}, документов: {}",
                v["total_nodes"], v["total_documents"]
            );
            println!(
                "по типам: {}",
                serde_json::to_string(&v["nodes_by_fact_type"]).unwrap_or_default()
            );
            println!(
                "операций в работе: {}, провалов: {}",
                v["pending_operations"], v["failed_operations"]
            );
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retain_args_split_flags_from_text() {
        let args: Vec<String> = [
            "--context",
            "работа над snens",
            "--tag",
            "x",
            "два",
            "слова",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (c, tags, text) = parse_retain(&args, &["agent:snens".to_string()]);
        assert_eq!(c, "работа над snens");
        assert_eq!(tags, vec!["agent:snens", "x"]);
        assert_eq!(text.join(" "), "два слова");
        // инлайн-форма и «-» (текст со stdin)
        let args: Vec<String> = ["--tag=y", "-"].iter().map(|s| s.to_string()).collect();
        let (_, tags, text) = parse_retain(&args, &[]);
        assert_eq!(tags, vec!["y"]);
        assert!(text.is_empty(), "«-» не должен попадать в текст");
    }

    #[test]
    fn turn_json_becomes_conversation() {
        let t = turn_text(r#"{"user":"почини лог","assistant":"готово"}"#).unwrap();
        assert_eq!(t, "User: почини лог\nAssistant: готово");
        // не ход — текст как есть
        assert!(turn_text("просто факт").is_none());
        assert!(turn_text(r#"{"другое":"поле"}"#).is_none());
    }
}
