// Разбор argv. Контракт фиксирован: fetch <url> [--raw] [--fallback …] [--save …]
// [--max-chars N] [--timeout S]; stdin не нужен.
use crate::die;

pub const USAGE: &str =
    "fetch <url> [--raw] [--fallback auto|never|always] [--save путь] [--max-chars N] [--timeout S]";

/// Справка целиком одним литералом: `scripts/docs-tools.py` собирает из неё
/// машинный `tools.txt`, и второй источник текста тут же разошёлся бы с первым.
pub const HELP: &str = "\
fetch — прочитать веб-страницу как markdown.
fetch <url> [--raw] [--fallback auto|never|always] [--save путь] [--max-chars N] [--timeout S]

Свой путь: GET с браузерным User-Agent, очистка DOM (script/nav/header/footer/…),
<main>/<article> в приоритете, затем markdown. Не-html (pdf и бинарь) сливается в файл.

  --raw              не чистить DOM, конвертировать всё body
  --fallback MODE    auto (дефолт) — r.jina.ai при 403/429 или пустом результате;
                     never — никогда; always — сразу только r.jina.ai
  --save путь        куда слить не-html (иначе каталог SNENS_ATTACH_DIR)
  --max-chars N      обрезать вывод (дефолт 40000)
  --timeout S        таймаут запроса, секунды (дефолт 30)

PDF при наличии системного pdftotext извлекается в текст (если нет --save).";

/// Текст, который печатает `--help`.
pub fn help() -> String {
    HELP.to_string()
}

#[derive(Clone, Copy, PartialEq)]
pub enum Fallback {
    Auto,
    Never,
    Always,
}

pub struct Args {
    pub url: String,
    pub raw: bool,
    pub fallback: Fallback,
    pub save: Option<String>,
    pub max_chars: usize,
    pub timeout: u64,
}

/// Флаги, которые значения не принимают: `--raw=1` — ошибка, а не «--raw плюс URL 1».
const BOOL_FLAGS: [&str; 1] = ["raw"];

/// `--max-chars=2000` → `["--max-chars", "2000"]`: один разбор вместо двух.
/// Булевы флаги не разворачиваются: иначе значение уехало бы в позиционные.
fn expand(argv: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for a in argv {
        match a.strip_prefix("--").and_then(|r| r.split_once('=')) {
            Some((k, _)) if BOOL_FLAGS.contains(&k) => die(
                &format!("fetch: --{k} значения не принимает\n{USAGE}"),
                2,
            ),
            Some((k, v)) => {
                out.push(format!("--{k}"));
                out.push(v.to_string());
            }
            None => out.push(a.clone()),
        }
    }
    out
}

pub fn parse(argv: &[String]) -> Args {
    let argv = expand(argv);
    let mut url: Option<String> = None;
    let mut a = Args {
        url: String::new(),
        raw: false,
        fallback: Fallback::Auto,
        save: None,
        max_chars: 40_000,
        timeout: 30,
    };
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        match arg {
            "--raw" => {
                a.raw = true;
                i += 1;
            }
            "--fallback" | "--save" | "--max-chars" | "--timeout" => {
                let v = argv
                    .get(i + 1)
                    .unwrap_or_else(|| die(&format!("fetch: {arg} без значения\n{USAGE}"), 2));
                match arg {
                    "--fallback" => {
                        a.fallback = match v.as_str() {
                            "auto" => Fallback::Auto,
                            "never" => Fallback::Never,
                            "always" => Fallback::Always,
                            other => die(
                                &format!(
                                    "fetch: --fallback ждёт auto|never|always, получено {other}\n{USAGE}"
                                ),
                                2,
                            ),
                        }
                    }
                    "--save" => a.save = Some(v.clone()),
                    "--max-chars" => a.max_chars = num(&argv, i, "--max-chars", 1, 10_000_000),
                    _ => a.timeout = num(&argv, i, "--timeout", 1, 600) as u64,
                }
                i += 2;
            }
            _ if arg.starts_with('-') => {
                die(&format!("fetch: неизвестный аргумент {arg}\n{USAGE}"), 2)
            }
            _ => {
                if url.is_some() {
                    die(&format!("fetch: URL указан дважды\n{USAGE}"), 2);
                }
                url = Some(arg.to_string());
                i += 1;
            }
        }
    }
    a.url = url.unwrap_or_else(|| die(&format!("fetch: не указан URL\n{USAGE}"), 2));
    a
}

/// Число из argv[i+1] в границах `lo..=hi`: ноль и абсурдные значения — ошибка,
/// иначе `--timeout 0` делает запрос мгновенно просроченным.
fn num(argv: &[String], i: usize, name: &str, lo: usize, hi: usize) -> usize {
    let Some(v) = argv.get(i + 1) else {
        die(&format!("fetch: {name} без значения\n{USAGE}"), 2);
    };
    let n: usize = v.parse().unwrap_or_else(|_| {
        die(
            &format!("fetch: {name} ждёт число, получено {v}\n{USAGE}"),
            2,
        )
    });
    if n < lo || n > hi {
        die(
            &format!(
                "fetch: {name} ждёт число {lo}..{hi}, получено {v}\n{USAGE}"
            ),
            2,
        );
    }
    n
}
