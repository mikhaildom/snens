// audit — разобрать лог сессии: таймлайн вызовов и подозрительные места.
// Формат лога не дублируем структурами: читаем как Value, поэтому новое поле
// в записи не ломает аудит.
use serde_json::Value;
use std::process::exit;

const HELP: &str = "\
audit — разобрать лог сессии: таймлайн вызовов и подозрительные места.
audit <лог.jsonl> [--huge N]

Таймлайн показывает, что и в каком порядке делалось: повторяемая форма работы видна
в нём и тогда, когда ошибок нет. Подозреваемые — детерминированные счётчики:
duplicate, retry-storm, huge, failed-abandoned. Разделы печатаются всегда: молчание
неотличимо от «не смотрел».";

const HUGE: usize = 20_000;
const CMD_WIDTH: usize = 110;

fn die(msg: String) -> ! {
    eprintln!("{msg}");
    exit(1);
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "-h" || argv[0] == "--help" {
        println!("{HELP}");
        exit(if argv.is_empty() { 1 } else { 0 });
    }
    let mut path: Option<String> = None;
    let mut huge = HUGE;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--huge" => {
                let v = argv
                    .get(i + 1)
                    .unwrap_or_else(|| die("audit: --huge без значения".into()));
                huge = v
                    .parse()
                    .unwrap_or_else(|_| die(format!("audit: --huge ждёт число, получено {v}")));
                i += 2;
            }
            a if a.starts_with('-') && a != "-" => die(format!("audit: неизвестный аргумент {a}")),
            a => {
                if path.is_some() {
                    die("audit: путь указан дважды".into());
                }
                path = Some(a.to_string());
                i += 1;
            }
        }
    }
    let path = path.unwrap_or_else(|| die("audit: не указан лог".into()));
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| die(format!("audit: {path}: {e}")));
    print!("{}", render(&analyze(&path, &text), huge));
}

struct Row {
    turn: usize,
    tool: String,
    /// человекочитаемая строка вызова (обрезана для печати)
    cmd: String,
    /// ключ повтора: полное содержимое вызова, без обрезки
    key: String,
    exit: i32,
    bytes: usize,
    ms: u64,
}

struct Report {
    path: String,
    records: usize,
    broken: usize,
    rows: Vec<Row>,
    turns: usize,
    in_tok: u64,
    cached_tok: u64,
    out_tok: u64,
    cost_micro: u64,
    elided: usize,
    freed_bytes: usize,
}

/// Разбор сырой строки аргументов от модели в пару (человекочитаемый `cmd`,
/// ключ повтора `key`). `argv[0]` — сырая строка аргументов; для глаза достаём
/// из неё `args` (у тулов) или `command` (у `run`), а ключом делаем **полное**
/// содержимое вызова, иначе две разные правки с общим 40-символьным префиксом
/// `old_string` склеиваются в ложный `duplicate`.
fn cmd_of(raw: &str) -> (String, String) {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return (raw.to_string(), raw.to_string());
    };
    let head = match v.get("args") {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| x.as_str().unwrap_or("?"))
            .collect::<Vec<_>>()
            .join(" "),
        _ => match v.get("command").and_then(Value::as_str) {
            Some(c) => c.to_string(),
            None => raw.to_string(),
        },
    };
    // Правка того же файла другим текстом — не повтор вызова: без этого два
    // разных `edit` неотличимы и попадают в duplicate.
    let mut full = head.clone();
    let mut shown = head;
    if let Some(old) = v.get("old_string").and_then(Value::as_str) {
        full.push_str(&format!("  ⟵ «{old}»"));
        shown.push_str(&format!("  ⟵ «{}»", clip(old, 40)));
    }
    if let Some(stdin) = v.get("stdin").and_then(Value::as_str) {
        // У тулов правки payload приходит на stdin, а не в args: без разбора
        // stdin два разных `edit` одного файла неотличимы.
        let old = serde_json::from_str::<Value>(stdin).ok().and_then(|s| {
            s.get("old_string")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        match old {
            Some(o) => {
                full.push_str(&format!("  ⟵ «{o}»"));
                shown.push_str(&format!("  ⟵ «{}»", clip(&o, 40)));
            }
            None => {
                let t = format!("  ⇐ {} Б на stdin", stdin.len());
                full.push_str(&t);
                shown.push_str(&t);
            }
        }
    }
    (shown, full)
}

fn clip(s: &str, n: usize) -> String {
    let mut t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        t.push('…');
    }
    t
}

fn analyze(path: &str, text: &str) -> Report {
    let mut r = Report {
        path: path.to_string(),
        records: 0,
        broken: 0,
        rows: Vec::new(),
        turns: 0,
        in_tok: 0,
        cached_tok: 0,
        out_tok: 0,
        cost_micro: 0,
        elided: 0,
        freed_bytes: 0,
    };
    // Вызовы текущего хода: результат приходит отдельной записью и знает только call_id.
    let mut pending: Vec<(String, String, String, String)> = Vec::new();

    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            r.broken += 1;
            continue;
        };
        r.records += 1;
        match v.get("kind").and_then(Value::as_str).unwrap_or("") {
            "assistant" => {
                r.turns += 1;
                for c in v
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .unwrap_or(&Vec::new())
                {
                    let id = c
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = c
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let (cmd, key) = cmd_of(c.get("args").and_then(Value::as_str).unwrap_or(""));
                    pending.push((id, name, cmd, key));
                }
            }
            "tool_result" => {
                let id = v.get("call_id").and_then(Value::as_str).unwrap_or("");
                let tool = v
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                let argv0 = v
                    .get("argv")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let found = pending.iter().position(|(cid, _, _, _)| cid == id);
                let (tool, cmd, key) = match found {
                    Some(pos) => {
                        let (_, n, c, k) = pending.remove(pos);
                        (n, c, k)
                    }
                    None => {
                        let (c, k) = cmd_of(argv0);
                        (tool, c, k)
                    }
                };
                r.rows.push(Row {
                    turn: r.turns,
                    tool,
                    cmd,
                    key,
                    exit: v.get("exit").and_then(Value::as_i64).unwrap_or(-1) as i32,
                    bytes: v.get("bytes").and_then(Value::as_u64).unwrap_or(0) as usize,
                    ms: v.get("ms").and_then(Value::as_u64).unwrap_or(0),
                });
            }
            "usage" => {
                r.in_tok += v.get("in_tok").and_then(Value::as_u64).unwrap_or(0);
                r.cached_tok += v.get("cached_tok").and_then(Value::as_u64).unwrap_or(0);
                r.out_tok += v.get("out_tok").and_then(Value::as_u64).unwrap_or(0);
                r.cost_micro += v.get("cost_micro").and_then(Value::as_u64).unwrap_or(0);
            }
            "ctx" => {
                r.elided += v.get("elided").and_then(Value::as_u64).unwrap_or(0) as usize;
                r.freed_bytes += v.get("freed_bytes").and_then(Value::as_u64).unwrap_or(0) as usize;
            }
            _ => {}
        }
    }
    r
}

/// Счётчики по вызовам: что делалось повторно, что вернуло много, что провалилось
/// и не было повторено. Всё выводится из лога — никакой модели здесь нет.
fn suspects(rows: &[Row], huge: usize) -> Vec<String> {
    let mut out = Vec::new();
    // ponytail: линейный поиск по ключу, лог на тысячи строк ещё не тот случай
    let mut groups: Vec<((&str, &str), Vec<usize>)> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        // Ключ повтора — полное содержимое вызова (r.key), а не обрезанное для
        // печати r.cmd: иначе две разные правки с общим префиксом old_string
        // дают ложный duplicate.
        let key = (r.tool.as_str(), r.key.as_str());
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, list)) => list.push(i),
            None => groups.push((key, vec![i])),
        }
    }
    for ((tool, cmd), idx) in &groups {
        if idx.len() >= 2 {
            let turns: Vec<String> = idx.iter().map(|i| rows[*i].turn.to_string()).collect();
            let what = format!("{tool}  {}", clip(cmd, CMD_WIDTH));
            if idx.len() >= 3 {
                out.push(format!(
                    "retry-storm      ×{}  ходы {}  {what}",
                    idx.len(),
                    turns.join(", ")
                ));
            } else {
                out.push(format!(
                    "duplicate        ×{}  ходы {}  {what}",
                    idx.len(),
                    turns.join(", ")
                ));
            }
        }
    }
    for r in rows.iter().filter(|r| r.bytes > huge) {
        out.push(format!(
            "huge             {} Б (> {huge})  ход {}  {}  {}",
            r.bytes,
            r.turn,
            r.tool,
            clip(&r.cmd, CMD_WIDTH)
        ));
    }
    for (i, r) in rows.iter().enumerate() {
        if r.exit == 0 {
            continue;
        }
        let retried_ok = rows
            .iter()
            .skip(i + 1)
            .any(|n| n.exit == 0 && n.tool == r.tool && n.key == r.key);
        if !retried_ok {
            out.push(format!(
                "failed-abandoned exit={:<4} ход {}  {}  {}",
                r.exit,
                r.turn,
                r.tool,
                clip(&r.cmd, CMD_WIDTH)
            ));
        }
    }
    out
}

fn render(r: &Report, huge: usize) -> String {
    let mut s = String::new();
    let error_rows = r.rows.iter().filter(|x| x.exit != 0).count();
    s.push_str(&format!(
        "лог: {} — записей {}, ходов {}, вызовов {}, с ошибкой {}",
        r.path,
        r.records,
        r.turns,
        r.rows.len(),
        error_rows
    ));
    if r.broken > 0 {
        s.push_str(&format!(", битых строк {}", r.broken));
    }
    s.push('\n');
    s.push_str(&format!(
        "токены: вход {} (кеш {}), выход {}, ${:.4}\n",
        r.in_tok,
        r.cached_tok,
        r.out_tok,
        r.cost_micro as f64 / 1e6
    ));
    if r.elided > 0 {
        s.push_str(&format!(
            "компакция: вырезано результатов {}, освобождено {} Б\n",
            r.elided, r.freed_bytes
        ));
    }

    s.push_str("\n## Таймлайн\n");
    if r.rows.is_empty() {
        s.push_str("   (вызовов инструментов не было)\n");
    }
    // Ширина колонки команды — по самой длинной в этом логе: пустые поля
    // читаются хуже, чем ровные.
    let w = r
        .rows
        .iter()
        .map(|x| clip(&x.cmd, CMD_WIDTH).chars().count())
        .max()
        .unwrap_or(0);
    for (n, row) in r.rows.iter().enumerate() {
        let flag = if row.exit != 0 { "!" } else { " " };
        s.push_str(&format!(
            "{:>3}.{flag} ход {:<3} {:<8} {:<w$}  exit={:<4} {:>8} Б {:>6} мс\n",
            n + 1,
            row.turn,
            row.tool,
            clip(&row.cmd, CMD_WIDTH),
            row.exit,
            row.bytes,
            row.ms
        ));
    }

    s.push_str("\n## Подозреваемые\n");
    let list = suspects(&r.rows, huge);
    if list.is_empty() {
        s.push_str("   (ничего)\n");
    }
    for l in list {
        s.push_str(&format!("   {l}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Фикстура синтетическая и собирается здесь: логи реальных сессий в
    /// публикуемый репозиторий не попадают.
    fn fixture() -> String {
        let mut s = String::new();
        let mut add = |v: Value| {
            s.push_str(&v.to_string());
            s.push('\n');
        };
        let call = |id: &str, tool: &str, args: &str| serde_json::json!({"kind":"assistant","ts":1,"text":null,"tool_calls":[{"id":id,"name":tool,"args":args}]});
        let result = |id: &str, tool: &str, exit: i32, bytes: usize| {
            serde_json::json!({"kind":"tool_result","ts":2,"call_id":id,"tool":tool,
                "argv":[format!("{{\"args\":\"{tool} x\"}}")],"exit":exit,"bytes":bytes,"ms":7,
                "body":{"t":"inline","text":"..."}})
        };
        add(serde_json::json!({"kind":"meta","ts":0,"model":"test"}));
        add(serde_json::json!({"kind":"user","ts":0,"text":"сделай","source":"cli"}));
        for (id, n) in [("c1", 1), ("c2", 2), ("c3", 3)] {
            if n == 3 {
                add(
                    serde_json::json!({"kind":"ctx","ts":1,"action":"elide","elided":2,"freed_bytes":5000}),
                );
            }
            add(call(
                id,
                "run",
                "{\"args\":[\"grep\",\"-n\",\"Rec\",\"src/*.rs\"]}",
            ));
            add(result(id, "run", 0, 400));
        }
        add(call("c4", "read", "{\"args\":\"src/main.rs\"}"));
        add(result("c4", "read", 0, 30_000));
        add(call("c5", "run", "{\"args\":[\"cargo\",\"test\"]}"));
        add(result("c5", "run", 101, 900));
        add(
            serde_json::json!({"kind":"usage","ts":3,"in_tok":1000,"cached_tok":800,"out_tok":50,"cost_micro":300}),
        );
        s
    }

    #[test]
    fn timeline_pairs_results_with_calls() {
        let r = analyze("t.jsonl", &fixture());
        assert_eq!(r.rows.len(), 5, "не все вызовы попали в таймлайн");
        assert_eq!(r.rows[0].cmd, "grep -n Rec src/*.rs", "args не разобраны");
        assert_eq!(r.rows[0].turn, 1);
        assert_eq!(r.rows[4].tool, "run");
        assert_eq!(r.turns, 5, "ходов должно быть по числу assistant-записей");
        assert_eq!(r.in_tok, 1000);
        assert_eq!(r.elided, 2);
    }

    #[test]
    fn counters_name_what_happened() {
        let r = analyze("t.jsonl", &fixture());
        let text = render(&r, HUGE);
        assert!(
            text.contains("retry-storm"),
            "три одинаковых вызова: {text}"
        );
        assert!(text.contains("huge"), "результат 30 КБ: {text}");
        assert!(
            text.contains("failed-abandoned"),
            "провал без успешного повтора: {text}"
        );
        assert!(
            !text.contains("duplicate"),
            "retry-storm и duplicate не дублируются"
        );
        assert!(
            text.contains("вырезано результатов 2"),
            "журнал компакции: {text}"
        );
    }

    #[test]
    fn clean_log_reports_nothing_suspicious() {
        let text = "{\"kind\":\"assistant\",\"ts\":1,\"text\":\"ок\",\"tool_calls\":[]}\n";
        let r = analyze("t.jsonl", &text);
        let out = render(&r, HUGE);
        assert!(out.contains("(ничего)"), "на чистом логе: {out}");
        assert!(out.contains("(вызовов инструментов не было)"));
    }

    /// Разные правки одного файла — не повтор вызова: без текста old_string
    /// они неотличимы и попадают в duplicate.
    #[test]
    fn different_edits_to_same_file_are_not_duplicates() {
        let mut s = String::new();
        for (id, old) in [("e1", "alpha"), ("e2", "beta")] {
            let inner = serde_json::json!({"old_string": old, "new_string": "x"}).to_string();
            let args = serde_json::json!({"args": "f.rs", "stdin": inner}).to_string();
            s.push_str(
                &serde_json::json!({"kind":"assistant","ts":1,"text":null,
                    "tool_calls":[{"id":id,"name":"edit","args":args}]})
                .to_string(),
            );
            s.push('\n');
            s.push_str(
                &serde_json::json!({"kind":"tool_result","ts":2,"call_id":id,"tool":"edit",
                    "argv":["{}"],"exit":0,"bytes":130,"ms":5,"body":{"t":"inline","text":"{}"}})
                .to_string(),
            );
            s.push('\n');
        }
        let out = render(&analyze("t.jsonl", &s), HUGE);
        assert!(
            !out.contains("duplicate"),
            "разные правки — не повтор: {out}"
        );
        assert!(out.contains("«alpha»"), "текст правки виден: {out}");
    }

    /// Регресс: у двух разных правок `old_string` совпадает в первых 40+ символах.
    /// Ключ повтора обязан брать полный текст, иначе — ложный duplicate.
    #[test]
    fn edits_sharing_a_long_prefix_are_not_duplicates() {
        let prefix = "pub struct Reply {\n    pub text: String,\n    pub calls: Vec<Call>,";
        assert!(prefix.chars().count() > 40);
        let mut s = String::new();
        for (id, tail) in [
            ("e1", "\n    pub extra: bool,"),
            ("e2", "\n    /// ход\n    pub note: String,"),
        ] {
            let old = format!("{prefix}{tail}");
            let inner = serde_json::json!({"old_string": old, "new_string": "x"}).to_string();
            let args = serde_json::json!({"args": "f.rs", "stdin": inner}).to_string();
            s.push_str(
                &serde_json::json!({"kind":"assistant","ts":1,"text":null,
                    "tool_calls":[{"id":id,"name":"edit","args":args}]})
                .to_string(),
            );
            s.push('\n');
            s.push_str(
                &serde_json::json!({"kind":"tool_result","ts":2,"call_id":id,"tool":"edit",
                    "argv":["{}"],"exit":0,"bytes":130,"ms":5,"body":{"t":"inline","text":"{}"}})
                .to_string(),
            );
            s.push('\n');
        }
        let out = render(&analyze("t.jsonl", &s), HUGE);
        assert!(
            !out.contains("duplicate"),
            "общий префикс old_string — не повтор: {out}"
        );
    }

    #[test]
    fn broken_line_is_counted_not_fatal() {
        let mut text = fixture();
        text.push_str("{это не json}\n");
        let r = analyze("t.jsonl", &text);
        assert_eq!(r.broken, 1);
        assert!(render(&r, HUGE).contains("битых строк 1"));
    }
}
