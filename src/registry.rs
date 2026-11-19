// Реестр тулов: каталог = тул, файл с тем же именем внутри — исполняемый.
// Определение тула для модели — его собственный `--help`, поэтому контракт жёсткий:
// строка 1 — суть, строка 2 — usage, дальше подробности.
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::{cfg::Config, exec, R};

#[derive(Debug, Clone)]
pub struct Tool {
    pub name: String,
    pub bin: PathBuf,
    pub summary: String,
    pub usage: String,
    /// Usage-строка хелпа несёт признак «нужен stdin»: тогда определение тула
    /// помечает stdin обязательным, и модель узнаёт контракт заранее, а не
    /// тратит ход на exit=1 при первом вызове.
    pub uses_stdin: bool,
    /// Строка полного хелпа, где тул говорит про stdin. Ей уезжает в описание
    /// даже когда stdin не обязателен: `memory` читает его одной подкоманде из
    /// восьми, и без этой строки модель узнаёт контракт только отказом.
    pub stdin_hint: Option<String>,
    /// Типизированный интерфейс из `schema.json` рядом с бинарником. Есть —
    /// модель видит настоящие поля, а ядро само собирает argv/stdin; нет —
    /// остаётся generic-контракт `args`/`stdin` (обратная совместимость).
    pub schema: Option<ToolSchema>,
}

/// Правило сборки одного argv-элемента из поля вызова.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgRule {
    /// Значение поля как отдельный аргумент.
    Field(String),
    /// `--flag value`, если поле задано; либо голый `--flag`, если `when_field` истинно.
    Flag {
        flag: String,
        field: Option<String>,
        when_field: Option<String>,
    },
    /// Литерал как есть.
    Const(String),
}

/// Что уходит на stdin процесса.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdinRule {
    /// stdin закрыт.
    None,
    /// Сырое значение поля, без JSON-обёртки (`write`: содержимое файла).
    Field(String),
    /// Объект из перечисленных полей, сериализованный serde_json (`edit`).
    Json(Vec<String>),
}

/// Разобранный `schema.json`: JSON Schema для модели + наш слой
/// «поля → argv/stdin» под ключом `x-snens`.
#[derive(Debug, Clone)]
pub struct ToolSchema {
    /// Готовый `parameters` для OpenAI-определения (без `x-snens`).
    pub parameters: Value,
    pub argv: Vec<ArgRule>,
    pub stdin: StdinRule,
}

impl ToolSchema {
    /// Поля схемы: имя → тип (для `snens tools` и сообщений об ошибке).
    pub fn fields(&self) -> Vec<(String, String)> {
        let props = self.parameters.get("properties").and_then(Value::as_object);
        let Some(props) = props else {
            return Vec::new();
        };
        props.iter().map(|(k, v)| (k.clone(), type_of(v))).collect()
    }

    pub fn field_names(&self) -> Vec<String> {
        self.fields().into_iter().map(|(n, _)| n).collect()
    }

    pub fn required(&self) -> Vec<String> {
        self.parameters
            .get("required")
            .and_then(Value::as_array)
            .map(|v| {
                v.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Тип поля строкой: `string`, `integer`, `array<object>` — ровно то, что
/// печатает `snens tools` и перечисляет в ошибке.
fn type_of(v: &Value) -> String {
    let base = match v.get("type") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("|"),
        _ => "any".into(),
    };
    match v.get("items").map(type_of) {
        Some(inner) if base == "array" => format!("array<{inner}>"),
        _ => base,
    }
}

pub async fn discover(cfg: &Config) -> Vec<Tool> {
    let dir = cfg.tools_path();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return out;
    };
    let mut dirs: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    for d in dirs {
        let Some(name) = d.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        let bin = d.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        if !bin.is_file() {
            // Каталог без бинарника — библиотечный крейт (`tools/cdp`: транспорт
            // для `shot` и `genui`), а не сломанный тул. Иначе каждый старт
            // писал бы по строке об ошибке про то, чего в реестре и не должно быть.
            if d.join("Cargo.toml").is_file() && !d.join("src/main.rs").is_file() {
                continue;
            }
            eprintln!(
                "[snens] тул {name}: нет исполняемого файла {}",
                bin.display()
            );
            continue;
        }
        let bin_s = bin.to_string_lossy().into_owned();
        let mut sp = exec::Spawn::argv(&bin_s, vec!["--help".into()]);
        sp.timeout_s = 5;
        sp.cap = 4000;
        let (summary, usage, help) = match exec::spawn(sp).await {
            Ok(o) if o.exit == 0 => {
                let (s, u) = split_help(&o.text);
                (s, u, o.text)
            }
            Ok(o) => {
                eprintln!("[snens] тул {name}: --help вернул код {}", o.exit);
                continue;
            }
            Err(e) => {
                eprintln!("[snens] тул {name}: {e}");
                continue;
            }
        };
        if summary.is_empty() {
            eprintln!("[snens] тул {name}: пустой --help");
            continue;
        }
        let uses_stdin = stdin_required(&summary, &usage);
        // Весь хелп, а не две первые строки: подсказка про stdin живёт ниже и
        // обязательной не делает — иначе у `memory` stdin потребовался бы и для
        // `recall`.
        let stdin_hint = stdin_line(&help);
        // schema.json рядом с бинарником: кривой файл — не повод терять тул,
        // он просто остаётся на generic-контракте, а причина видна в stderr.
        let schema = match read_schema(&d.join("schema.json")) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[snens] тул {name}: schema.json не принят: {e}");
                None
            }
        };
        out.push(Tool {
            name,
            bin,
            summary,
            usage,
            uses_stdin,
            stdin_hint,
            schema,
        });
    }
    out
}

/// Первая непустая строка — суть, вторая — usage.
fn split_help(text: &str) -> (String, String) {
    let mut it = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let summary = it.next().unwrap_or_default().to_string();
    let usage = it.next().unwrap_or_default().to_string();
    (summary, usage)
}

/// Нужен ли тулу stdin — видно из сути и usage-строки хелпа (`<путь>   — JSON на
/// stdin`, `содержимое берётся со stdin`). Признак ставит сам тул, и он же в
/// `--help`: отдельной схемы на тип тула не заводим.
fn stdin_required(summary: &str, usage: &str) -> bool {
    summary.contains("stdin") || usage.contains("stdin")
}

/// Строка хелпа, где тул сам говорит про stdin (`memory`: «ход (JSON {"user",
/// "assistant"} на stdin)»). Ей уезжает в описание тула — точнее общей фразы.
fn stdin_line(help: &str) -> Option<String> {
    help.lines()
        .find(|l| l.contains("stdin"))
        .map(|l| l.trim().to_string())
}

/// `schema.json` → ToolSchema. Отсутствующий файл — `None` (generic-контракт),
/// битый — Err (тул остаётся generic, причина в stderr).
pub fn read_schema(path: &Path) -> Result<Option<ToolSchema>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{e}")),
    };
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("не JSON: {e}"))?;
    let ext = v
        .get("x-snens")
        .and_then(Value::as_object)
        .ok_or("нет ключа x-snens")?;
    let argv = parse_argv(ext.get("argv"))?;
    let stdin = parse_stdin(ext.get("stdin"))?;
    // В parameters отдаём схему без нашего расширения: провайдер про x-snens
    // ничего не знает, а незнакомый ключ в JSON Schema — лишний риск.
    let mut parameters = v.clone();
    if let Some(o) = parameters.as_object_mut() {
        o.remove("x-snens");
    }
    if parameters.get("type").and_then(Value::as_str) != Some("object") {
        return Err("type должен быть object".into());
    }
    if !parameters.get("properties").is_some_and(Value::is_object) {
        return Err("нет properties".into());
    }
    Ok(Some(ToolSchema {
        parameters,
        argv,
        stdin,
    }))
}

fn parse_argv(v: Option<&Value>) -> Result<Vec<ArgRule>, String> {
    let Some(items) = v.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for it in items {
        let o = it.as_object().ok_or("argv: элемент не объект")?;
        let get = |k: &str| o.get(k).and_then(Value::as_str).map(str::to_string);
        let rule = match (get("field"), get("flag"), get("const")) {
            // {"field": X} — значение поля отдельным аргументом
            (Some(f), None, None) => ArgRule::Field(f),
            // {"flag": F, "field": X} — два элемента, если поле задано
            (Some(f), Some(flag), None) => ArgRule::Flag {
                flag,
                field: Some(f),
                when_field: get("when_field"),
            },
            // {"flag": F, "when_field": B} — голый флаг, если B истинно
            (None, Some(flag), None) => ArgRule::Flag {
                flag,
                field: None,
                when_field: get("when_field"),
            },
            (None, None, Some(c)) => ArgRule::Const(c),
            _ => return Err(format!("argv: непонятный элемент {it}")),
        };
        out.push(rule);
    }
    Ok(out)
}

fn parse_stdin(v: Option<&Value>) -> Result<StdinRule, String> {
    let Some(o) = v.and_then(Value::as_object) else {
        return Ok(StdinRule::None);
    };
    match o.get("mode").and_then(Value::as_str) {
        None | Some("none") => Ok(StdinRule::None),
        Some("field") => o
            .get("field")
            .and_then(Value::as_str)
            .map(|f| StdinRule::Field(f.to_string()))
            .ok_or_else(|| "stdin mode=field без field".to_string()),
        Some("json") => {
            let fields = o
                .get("fields")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            Ok(StdinRule::Json(fields))
        }
        Some(other) => Err(format!("stdin: неизвестный mode {other}")),
    }
}

/// Одна generic-схема на все тулы: аргументы — argv, документация — хелп.
pub fn definitions(tools: &[Tool]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            // Типизированный тул: параметры — его собственный schema.json,
            // описание — хелп (строка 1 — суть, строка 2 — usage).
            if let Some(s) = &t.schema {
                let mut desc = format!("{}\n{}", t.summary, t.usage);
                let fields = s
                    .fields()
                    .into_iter()
                    .map(|(n, ty)| format!("{n}: {ty}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                desc.push_str(&format!("\nполя: {fields}"));
                return json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": desc,
                        "parameters": s.parameters.clone(),
                    }
                });
            }
            let mut desc = format!("{}\n{}", t.summary, t.usage);
            // Требование stdin доезжает до модели из описания, а не из exit=1
            // на первом вызове: это часть контракта тула, не сюрприз. Берём
            // строку самого тула — она точнее общей фразы: у `memory` stdin
            // нужен одному подкомандиру из восьми и ждёт ход, а не «JSON правки».
            let own = t.stdin_hint.clone();
            if t.uses_stdin {
                desc.push_str(&format!(
                    "\nstdin обязателен: {}",
                    own.unwrap_or_else(|| "JSON правки / содержимое файла.".into())
                ));
            } else if let Some(line) = own {
                // Не обязателен, но тул его читает: без этой строки модель
                // узнаёт про stdin только отказом.
                desc.push_str(&format!("\nstdin: {line}"));
            }
            let required: Vec<&str> = if t.uses_stdin {
                vec!["args", "stdin"]
            } else {
                vec!["args"]
            };
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": desc,
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "args": {
                                "type": ["array", "string"],
                                "items": {"type": "string"},
                                "description": "Аргументы argv. Предпочитай массив: строка разбивается по пробелам и пробел внутри аргумента так не передать"
                            },
                            "stdin": {"type": "string", "description": "Что подать на stdin"},
                            "timeout_seconds": {"type": "integer"}
                        },
                        "required": required
                    }
                }
            })
        })
        .collect()
}

/// Симлинки в один каталог: так тулы попадают в PATH и харнесса, и человека.
/// Каталог наш, но чужие файлы не трогаем — удаляем только симлинки.
pub fn sync_bin_dir(cfg: &Config, tools: &[Tool]) -> R<PathBuf> {
    let bin = cfg.bin_path();
    // Пустой список — это не «тулов нет, почисти каталог», а «искать было негде»:
    // неверный `tools_dir`, чужой конфиг, запуск из другого каталога. Чистка в
    // этом случае убивает рабочий каталог живого инстанса — а `bin_path()` при
    // `--config` попадает в общий `%APPDATA%\snens\bin`, то есть в чужое место.
    // Поймано живьём: проверка с временным конфигом стёрла `bin` установки.
    if tools.is_empty() {
        return Ok(bin);
    }
    std::fs::create_dir_all(&bin)?;
    // Каталог наш; чистим только то, что сами и создали — на unix симлинки,
    // на Windows копии (симлинк там требует прав).
    let names: Vec<String> = tools
        .iter()
        .map(|t| format!("{}{}", t.name, std::env::consts::EXE_SUFFIX))
        .collect();
    for e in std::fs::read_dir(&bin)?.flatten() {
        let p = e.path();
        let own_link = p
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        let own_copy = !own_link
            && p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| !names.iter().any(|x| x == n))
                .unwrap_or(false);
        if own_link || own_copy {
            let _ = std::fs::remove_file(&p);
        }
    }
    for (t, name) in tools.iter().zip(&names) {
        // Тул с именем системной команды в PATH не выкладываем: `run 'grep -n'`
        // должен звать системный grep, а наш тул ядро запускает по абсолютному
        // пути. Иначе одна привычная команда в сессии ломается на каждой попытке.
        if shadows_system_command(&t.name) {
            continue;
        }
        place(&t.bin, &bin.join(name))?;
    }
    Ok(bin)
}

/// Положить тул в `bin`: на unix симлинк, на Windows жёсткая ссылка (прав не
/// требует), а если ФС ссылок не умеет — копия.
///
/// Повторный вызов обязан быть пустым, а `sync_bin_dir` зовётся на каждом ходу.
/// Раньше первым шагом стоял `hard_link`, и на втором вызове он падал с
/// «файл уже существует», уходил в `fs::copy` — а копирование файла в его
/// собственную жёсткую ссылку на Windows и есть os error 32 («файл занят другим
/// процессом»). Первый ход после установки работал (каталог `bin` создавался),
/// а каждый следующий ход падал на подготовке тулов, и в клиенте это выглядело
/// как «запись файла сессии не работает».
fn place(from: &Path, to: &Path) -> R<()> {
    if already_in_place(from, to) {
        return Ok(());
    }
    // Имя занято, но содержимое другое (копия от установщика, старая версия,
    // ссылка в никуда) — снимаем: следующий шаг создаст ссылку заново.
    if to.symlink_metadata().is_ok() {
        if let Err(error) = std::fs::remove_file(to) {
            // Имя держит работающий процесс (тул зовут из `bin`) — это не повод
            // ронять ход: каталог обновится на следующей попытке.
            eprintln!("[snens] {} не обновлён: {error}", to.display());
            return Ok(());
        }
    }
    #[cfg(unix)]
    {
        if let Err(error) = std::os::unix::fs::symlink(from, to) {
            // ФС без симлинков — кладём копию, лишь бы тул был в PATH.
            eprintln!(
                "[snens] {} — симлинк не создан, копирую: {error}",
                to.display()
            );
            std::fs::copy(from, to)?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        if std::fs::hard_link(from, to).is_err() {
            std::fs::copy(from, to)?;
        }
        Ok(())
    }
}

/// Тул уже на месте: та же ссылка (жёсткая, симлинк) или та же копия.
///
/// Сравнение по размеру и mtime: этого достаточно, потому что два разных
/// бинарника с совпадением до наносекунды не встречаются, а читать 17 МБ
/// тулов на каждом ходу ради сверки содержимого нельзя.
fn already_in_place(from: &Path, to: &Path) -> bool {
    let Ok(a) = std::fs::metadata(from) else {
        return false;
    };
    let Ok(b) = std::fs::metadata(to) else {
        return false;
    };
    let same = a.len() == b.len() && a.modified().ok() == b.modified().ok();
    same && a.modified().is_ok()
}

/// PATH детей: разделитель путей свой у каждой платформы, склеиваем им. Каталог
/// самого харнесса тоже в PATH — иначе изнутри сессии не позвать `snens sessions`.
/// PATH детей с дополнительными каталогами В НАЧАЛЕ (каталог шимов: `sudo` →
/// `sudo -A`): шим должен затенять системный sudo, но только у процессов-
/// инструментов агента.
///
/// Одно правило поверх этого: **каталог с тулом, чьё имя совпадает с системной
/// командой, в PATH детей не попадает**. `run` запускает честную оболочку, и
/// `grep -n`, `write`, `view` в ней обязаны оставаться системными. Тулы же ядро
/// запускает по абсолютному пути (`Tool.bin`) — PATH им для этого не нужен.
/// Системные каталоги исключение: `/usr/bin` не фильтруется никогда, иначе
/// пропадает оболочка и `run` падает «No such file or directory» (поймано живьём).
pub fn child_path_with(prefix: &[PathBuf], bin: &Path, tools: &[Tool]) -> String {
    let shadowed: Vec<String> = tools
        .iter()
        .filter(|t| shadows_system_command(&t.name))
        .map(|t| format!("{}{}", t.name, std::env::consts::EXE_SUFFIX))
        .collect();

    let mut parts: Vec<PathBuf> = prefix.to_vec();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        parts.push(dir);
    }
    // Пользовательские bin-каталоги — до PATH родителя. Когда харнесс поднят
    // systemd-юнитом (веб-поверхность), его PATH системный: `~/.cargo/bin`,
    // `~/.local/bin`, `~/.bun/bin` в нём отсутствуют, и `run cargo build` внутри
    // сессии падал exit=127, хотя в терминале человека cargo работает. Берём
    // каталоги из HOME, а не из шелла: тогда поведение одинаково для любой
    // поверхности (сервис, TUI, телеграм).
    parts.extend(user_bin_dirs());
    if let Some(cur) = std::env::var_os("PATH") {
        parts.extend(std::env::split_paths(&cur));
    }
    parts.push(bin.to_path_buf());

    let mut seen: Vec<PathBuf> = Vec::new();
    let parts: Vec<PathBuf> = parts
        .into_iter()
        .filter(|p| {
            if seen.contains(p) {
                return false;
            }
            seen.push(p.clone());
            true
        })
        .filter(|p| is_system_dir(p) || !has_shadow(p, &shadowed))
        .collect();
    std::env::join_paths(parts)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// В каталоге лежит файл с системным именем нашего тула — каталог в PATH детей
/// не отдаём. Проверяем именно имена НАШИХ тулов, а не вообще всё: `edit`,
/// `fetch`, `search` системных двойников не имеют и остаются доступны.
fn has_shadow(dir: &Path, shadowed: &[String]) -> bool {
    shadowed.iter().any(|name| dir.join(name).exists())
}

/// Пользовательские каталоги с бинарниками: то, что человек получает в PATH
/// логин-шелла (`~/.bashrc`/`~/.profile` экспортируют cargo, bun, nix), но не
/// получает процесс, поднятый systemd. Только существующие каталоги: мёртвый
/// путь в PATH ничего не стоит, но и смысла в нём нет.
fn user_bin_dirs() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    [
        ".cargo/bin",
        ".local/bin",
        "bin",
        ".bun/bin",
        ".npm-global/bin",
    ]
    .iter()
    .map(|p| home.join(p))
    .filter(|p| p.is_dir())
    .collect()
}

/// Системные каталоги: там настоящие утилиты, фильтровать их нельзя.
fn is_system_dir(p: &Path) -> bool {
    matches!(
        p.to_str(),
        Some("/bin")
            | Some("/sbin")
            | Some("/usr/bin")
            | Some("/usr/sbin")
            | Some("/usr/local/bin")
            | Some("/usr/local/sbin")
    )
}

/// Имя тула совпадает с системной командой — симлинк под этим именем в каталоге
/// тулов не создаём (и старый убираем). Модель зовёт тул по имени из схемы, но
/// это диспатч ядра по абсолютному пути; PATH ему для этого не нужен.
pub fn shadows_system_command(name: &str) -> bool {
    const SYSTEM: &[&str] = &[
        "grep", "find", "ls", "cat", "sed", "awk", "sort", "head", "tail", "cut", "date", "view",
        "write", "test", "patch", "diff", "echo", "printf", "which", "tee", "wc", "tr", "xargs",
        "mv", "cp", "rm", "true", "false", "yes", "seq", "basename", "dirname",
    ];
    SYSTEM.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "snens-reg-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn tool(name: &str, schema: Option<ToolSchema>) -> Tool {
        Tool {
            name: name.into(),
            bin: PathBuf::from(format!("/tmp/{name}")),
            summary: format!("{name} — суть"),
            usage: format!("{name} <args>"),
            uses_stdin: false,
            stdin_hint: None,
            schema,
        }
    }

    fn schema_from(text: &str) -> Option<ToolSchema> {
        let d = tmpdir("schema");
        let p = d.join("schema.json");
        std::fs::write(&p, text).unwrap();
        read_schema(&p).unwrap()
    }

    /// Подсказка про stdin берётся из полного хелпа, а не из двух первых строк:
    /// у `memory` тело на stdin нужно одной подкоманде из восьми, и без этой
    /// строки контракт добывался только отказом (`memory-retain-stdin-contract`).
    /// Обязательным он при этом не становится — `recall` живёт без stdin.
    #[test]
    fn stdin_hint_comes_from_full_help_without_making_stdin_required() {
        let help = "memory — долгая память\nmemory recall <запрос> | memory retain\n\n\
                    recall  — поиск.\n\
                    retain  — ход (JSON {\"user\",\"assistant\"} на stdin) в очередь.\n";
        assert_eq!(
            stdin_line(help).as_deref(),
            Some("retain  — ход (JSON {\"user\",\"assistant\"} на stdin) в очередь.")
        );
        // Две первые строки stdin не называют — значит и обязательным он не станет.
        let (summary, usage) = split_help(help);
        assert!(!stdin_required(&summary, &usage));
        assert!(stdin_line("write — файл\nwrite <путь>").is_none());
    }

    /// Каталог тулов уезжает в PATH детей В КОНЦЕ, а каталог, из которого по
    /// имени зовётся тул с системным именем, в PATH не попадает вовсе.
    #[test]
    fn tool_dir_is_last_and_shadowing_dirs_are_dropped() {
        let bin = PathBuf::from("/opt/tools-bin");
        let shim = PathBuf::from("/opt/shim");
        let path = child_path_with(std::slice::from_ref(&shim), &bin, &[]);
        let parts: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(parts.first(), Some(&shim), "шимы должны быть первыми");
        assert_eq!(
            parts.last(),
            Some(&bin),
            "каталог тулов должен быть последним"
        );
        assert_eq!(
            parts.iter().filter(|p| **p == bin).count(),
            1,
            "каталог тулов не дублируется: {path}"
        );
    }

    /// Тулы с именем системной команды распознаются по имени.
    #[test]
    fn shadowing_names_are_known() {
        for n in ["grep", "write", "view", "find", "ls"] {
            assert!(shadows_system_command(n), "{n} — системное имя");
        }
        for n in ["edit", "fetch", "search", "agent", "audit"] {
            assert!(!shadows_system_command(n), "{n} — не системная команда");
        }
    }

    /// Каталог с файлом системного имени (наш тул или копия в `cargo/debug`)
    /// уходит из PATH детей; системный `/usr/bin` остаётся — иначе пропадает
    /// оболочка и `run` падает «No such file or directory».
    #[test]
    fn dir_with_shadowing_name_is_dropped_but_system_dir_kept() {
        let dir = tmpdir("shadow-dir");
        let toolbin = dir.join("debug");
        std::fs::create_dir_all(&toolbin).unwrap();
        let names = vec![format!("grep{}", std::env::consts::EXE_SUFFIX)];
        std::fs::write(toolbin.join(&names[0]), b"x").unwrap();
        assert!(has_shadow(&toolbin, &names));
        // Чистый каталог остаётся.
        let clean = dir.join("clean");
        std::fs::create_dir_all(&clean).unwrap();
        assert!(!has_shadow(&clean, &names));
        // Системный каталог не фильтруется никогда.
        assert!(is_system_dir(Path::new("/usr/bin")));
        assert!(is_system_dir(Path::new("/bin")));
        assert!(!is_system_dir(Path::new("/home/x/bin")));
        // Имена наших безопасных тулов фильтр не включают.
        assert!(!has_shadow(&clean, &[]));
    }

    /// Симлинк под системным именем каталог тулов не создаёт: PATH не должен
    /// менять смысл `run`.
    #[test]
    fn tool_shadowing_system_command_is_not_linked() {
        let dir = tmpdir("shadow");
        let tools_dir = dir.join("tools");
        std::fs::create_dir_all(tools_dir.join("grep")).unwrap();
        std::fs::create_dir_all(tools_dir.join("edit")).unwrap();
        std::fs::write(tools_dir.join("grep/grep"), b"x").unwrap();
        std::fs::write(tools_dir.join("edit/edit"), b"x").unwrap();
        let cfg = Config {
            tools_dir: tools_dir.display().to_string(),
            ..Default::default()
        };
        let mut grep = tool("grep", None);
        grep.bin = tools_dir.join("grep/grep");
        let mut edit = tool("edit", None);
        edit.bin = tools_dir.join("edit/edit");
        let bin = sync_bin_dir(&cfg, &[grep, edit]).unwrap();
        assert!(
            !bin.join(format!("grep{}", std::env::consts::EXE_SUFFIX))
                .exists(),
            "симлинк на grep не должен появиться"
        );
        assert!(
            bin.join(format!("edit{}", std::env::consts::EXE_SUFFIX))
                .exists(),
            "обычный тул должен быть в PATH"
        );
    }

    /// Повторный `sync_bin_dir` — пустая операция. Он зовётся на КАЖДОМ ходу,
    /// а раньше падал на втором: `hard_link` под Windows отвечает «файл уже
    /// существует», откат в `fs::copy` копирует файл в его же жёсткую ссылку и
    /// получает os error 32. Из-за этого второй ход в установке на Windows не
    /// начинался вовсе, и в клиенте это выглядело ошибкой записи сессии.
    #[test]
    fn bin_dir_sync_is_idempotent() {
        let dir = tmpdir("idem");
        let tools_dir = dir.join("tools");
        std::fs::create_dir_all(tools_dir.join("read")).unwrap();
        std::fs::write(tools_dir.join("read/read"), b"x").unwrap();
        let cfg = Config {
            tools_dir: tools_dir.display().to_string(),
            ..Default::default()
        };
        let mut read = tool("read", None);
        read.bin = tools_dir.join("read/read");
        let bin = sync_bin_dir(&cfg, std::slice::from_ref(&read)).unwrap();
        let placed = bin.join(format!("read{}", std::env::consts::EXE_SUFFIX));
        assert!(placed.exists(), "первый вызов ставит тул");
        sync_bin_dir(&cfg, std::slice::from_ref(&read)).expect("второй вызов не падает");
        assert!(placed.exists(), "тул остаётся на месте");
        // Обновлённый тул (другой размер) переезжает в `bin` без ошибки.
        std::fs::write(tools_dir.join("read/read"), b"new-tool-bytes").unwrap();
        sync_bin_dir(&cfg, std::slice::from_ref(&read)).expect("обновление не падает");
        assert_eq!(std::fs::read(&placed).unwrap(), b"new-tool-bytes");
    }

    /// Пустой список тулов не должен чистить `bin`: это не «тулов нет», а
    /// «искать было негде». Случай не теоретический — проверка с временным
    /// конфигом стёрла рабочий `bin` установки.
    #[test]
    fn empty_tool_list_does_not_wipe_bin_dir() {
        let dir = tmpdir("empty");
        let tools_dir = dir.join("tools");
        std::fs::create_dir_all(&tools_dir).unwrap();
        let cfg = Config {
            tools_dir: tools_dir.display().to_string(),
            ..Default::default()
        };
        let bin = cfg.bin_path();
        std::fs::create_dir_all(&bin).unwrap();
        let keep = bin.join(format!("read{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&keep, b"x").unwrap();
        sync_bin_dir(&cfg, &[]).unwrap();
        assert!(keep.exists(), "пустой список стёр рабочий каталог тулов");
    }

    /// Пользовательские bin-каталоги (cargo, local/bin) попадают в PATH детей
    /// даже когда родитель поднят системным менеджером с системным PATH:
    /// иначе `run cargo build` из веб-сессии даёт exit=127, а в терминале
    /// человека тот же cargo работает.
    #[test]
    fn user_bin_dirs_are_in_child_path() {
        let home = tmpdir("home");
        std::fs::create_dir_all(home.join(".cargo/bin")).unwrap();
        std::fs::create_dir_all(home.join(".local/bin")).unwrap();
        // Каталог, которого нет, в PATH не уезжает: мёртвый путь только мусорит.
        let saved = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        let dirs = user_bin_dirs();
        match saved {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        assert!(
            dirs.contains(&home.join(".cargo/bin")),
            "cargo не в PATH: {dirs:?}"
        );
        assert!(
            dirs.contains(&home.join(".local/bin")),
            "local/bin не в PATH: {dirs:?}"
        );
        assert!(
            !dirs.contains(&home.join(".bun/bin")),
            "несуществующий каталог в PATH"
        );
    }

    #[test]
    fn no_schema_file_means_generic() {
        let d = tmpdir("none");
        assert!(read_schema(&d.join("schema.json")).unwrap().is_none());
        // Тул без схемы остаётся на generic args/stdin — обратная совместимость.
        let defs = definitions(&[tool("fetch", None)]);
        let p = &defs[0]["function"]["parameters"];
        assert!(p["properties"]["args"].is_object(), "{p}");
        assert!(p["properties"]["stdin"].is_object(), "{p}");
        assert_eq!(defs[0]["function"]["parameters"]["required"][0], "args");
    }

    #[test]
    fn schema_replaces_generic_parameters() {
        let s = schema_from(
            r#"{"type":"object","properties":{
                 "path":{"type":"string","description":"файл"},
                 "limit":{"type":"integer"}},
               "required":["path"],
               "x-snens":{"argv":[{"field":"path"},
                                   {"flag":"--limit","field":"limit"}],
                          "stdin":{"mode":"none"}}}"#,
        )
        .expect("схема не разобралась");
        assert_eq!(s.required(), vec!["path"]);
        assert_eq!(
            s.fields(),
            vec![
                ("limit".to_string(), "integer".to_string()),
                ("path".to_string(), "string".to_string())
            ]
        );
        assert_eq!(
            s.argv,
            vec![
                ArgRule::Field("path".into()),
                ArgRule::Flag {
                    flag: "--limit".into(),
                    field: Some("limit".into()),
                    when_field: None
                }
            ]
        );
        assert_eq!(s.stdin, StdinRule::None);

        let defs = definitions(&[tool("read", Some(s))]);
        let p = &defs[0]["function"]["parameters"];
        // Никакого второго уровня: args/stdin из generic-схемы исчезли.
        assert!(p["properties"]["args"].is_null(), "{p}");
        assert!(p["properties"]["stdin"].is_null(), "{p}");
        assert_eq!(p["properties"]["path"]["type"], "string");
        assert_eq!(p["required"][0], "path");
        // x-snens провайдеру не уезжает.
        assert!(p.get("x-snens").is_none(), "{p}");
    }

    #[test]
    fn stdin_modes_parse() {
        let field = schema_from(
            r#"{"type":"object","properties":{"content":{"type":"string"}},
                "x-snens":{"argv":[],"stdin":{"mode":"field","field":"content"}}}"#,
        )
        .unwrap();
        assert_eq!(field.stdin, StdinRule::Field("content".into()));
        let json = schema_from(
            r#"{"type":"object","properties":{"edits":{"type":"array"}},
                "x-snens":{"argv":[],"stdin":{"mode":"json","fields":["edits"]}}}"#,
        )
        .unwrap();
        assert_eq!(json.stdin, StdinRule::Json(vec!["edits".into()]));
    }

    #[test]
    fn broken_schema_is_reported_not_fatal() {
        let d = tmpdir("broken");
        let p = d.join("schema.json");
        std::fs::write(&p, "{не json").unwrap();
        assert!(read_schema(&p).is_err());
        // Без x-snens схема не годится: ядро не знает, как собрать argv.
        std::fs::write(&p, r#"{"type":"object","properties":{}}"#).unwrap();
        assert!(read_schema(&p).is_err());
    }

    #[test]
    fn array_type_is_described_with_items() {
        let s = schema_from(
            r#"{"type":"object","properties":{
                 "edits":{"type":"array","items":{"type":"object"}}},
               "x-snens":{"argv":[],"stdin":{"mode":"json","fields":["edits"]}}}"#,
        )
        .unwrap();
        assert_eq!(
            s.fields(),
            vec![("edits".to_string(), "array<object>".to_string())]
        );
    }
}
