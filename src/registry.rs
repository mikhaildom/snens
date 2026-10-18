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
        let props = self
            .parameters
            .get("properties")
            .and_then(Value::as_object);
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
        let (summary, usage) = match exec::spawn(sp).await {
            Ok(o) if o.exit == 0 => split_help(&o.text),
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
            // на первом вызове: это часть контракта тула, не сюрприз.
            let required: Vec<&str> = if t.uses_stdin {
                desc.push_str("\nstdin обязателен: JSON правки / содержимое файла.");
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
        let to = bin.join(name);
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(&t.bin, &to);
        #[cfg(not(unix))]
        std::fs::copy(&t.bin, &to)?;
    }
    Ok(bin)
}

/// PATH детей: разделитель путей свой у каждой платформы, склеиваем им. Каталог
/// самого харнесса тоже в PATH — иначе изнутри сессии не позвать `snens sessions`.
/// PATH детей с дополнительными каталогами В НАЧАЛЕ (каталог шимов: `sudo` →
/// `sudo -A`). Порядок важен: шим должен затенять системный sudo, но только у
/// процессов-инструментов агента, а не в обычной оболочке пользователя.
pub fn child_path_with(prefix: &[PathBuf], bin: &Path) -> String {
    let mut parts: Vec<PathBuf> = prefix.to_vec();
    parts.push(bin.to_path_buf());
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        parts.push(dir);
    }
    if let Some(cur) = std::env::var_os("PATH") {
        parts.extend(std::env::split_paths(&cur));
    }
    std::env::join_paths(parts)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
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
            schema,
        }
    }

    fn schema_from(text: &str) -> Option<ToolSchema> {
        let d = tmpdir("schema");
        let p = d.join("schema.json");
        std::fs::write(&p, text).unwrap();
        read_schema(&p).unwrap()
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
        assert_eq!(s.fields(), vec![("edits".to_string(), "array<object>".to_string())]);
    }
}
