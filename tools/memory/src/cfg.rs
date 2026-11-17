//! Настройки плагина: env → файл → дефолт. Файл рядом с конфигом харнесса
//! (`~/.config/snens/memory.toml`), ключ — из его `.env`, как у ядра.

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize, Default)]
// Опечатка в ключе (`embed_dimm`) иначе молча давала бы дефолт: с этим атрибутом
// разбор файла падает и мы говорим об этом.
#[serde(deny_unknown_fields)]
struct File {
    db: Option<String>,
    api_base: Option<String>,
    api_key_env: Option<String>,
    embed_model: Option<String>,
    embed_dim: Option<usize>,
    distill_model: Option<String>,
    rerank: Option<String>,
    rerank_model: Option<String>,
    max_facts: Option<usize>,
    max_tokens: Option<usize>,
}

pub struct Cfg {
    pub db: PathBuf,
    pub api_base: String,
    pub api_key: String,
    pub embed_model: String,
    pub embed_dim: usize,
    pub distill_model: String,
    pub rerank_model: Option<String>,
    pub max_facts: usize,
    pub max_tokens: usize,
}

fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(format!("{home}/.config/snens/memory.toml"))
}

/// Ключ провайдера: переменная окружения, затем `.env` рядом с конфигом.
/// Порядок обратный ядру (`SNENS_API_KEY` — рабочий ключ Nous) намеренно:
/// в `.bashrc` у нас лежит мёртвый `OPENROUTER_API_KEY`, и env бы его выиграл.
fn key_from_file(env_name: &str) -> Option<String> {
    let path = config_path().with_file_name(".env");
    let text = std::fs::read_to_string(path).ok()?;
    // Префикс считается один раз, а не на каждой строке файла.
    let prefix = format!("{env_name}=");
    for line in text.lines() {
        // `export KEY=…` — обычная форма в .env, и она тоже должна работать.
        let line = line.trim().strip_prefix("export ").unwrap_or(line.trim());
        let Some(rest) = line.strip_prefix(&prefix) else {
            continue;
        };
        let v = rest.trim().trim_matches('"').trim_matches('\'').to_string();
        if !v.is_empty() {
            return Some(v);
        }
    }
    None
}

/// Разбор `memory.toml`. Ошибку не глотаем: опечатка в конфиге иначе молча
/// оставляла бы пользователя с чужим `api_base`/`db`, и это трудно заметить.
fn read_file() -> File {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return File::default(),
        Err(e) => {
            eprintln!("memory: {}: {e} — беру настройки по умолчанию", path.display());
            return File::default();
        }
    };
    match toml::from_str(&text) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "memory: {} не разобран: {e} — беру настройки по умолчанию",
                path.display()
            );
            File::default()
        }
    }
}

/// Путь базы без сети и ключа: `retain`/`forget`/`stats` работают локально,
/// и падать на отсутствующем ключе им незачем.
pub fn db_path() -> PathBuf {
    default_db_path(&read_file())
}

fn default_db_path(f: &File) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(
        f.db.clone()
            .unwrap_or_else(|| format!("{home}/.local/share/snens/memory.db")),
    )
}

impl Cfg {
    pub fn load() -> Result<Cfg, String> {
        let f = read_file();
        let key_env = f.api_key_env.clone().unwrap_or_else(|| "SNENS_API_KEY".into());
        let api_key = std::env::var(&key_env)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| key_from_file(&key_env))
            .ok_or_else(|| format!("нет ключа {key_env} (env или ~/.config/snens/.env)"))?;
        Ok(Cfg {
            // Путь считаем из уже разобранного файла: повторное чтение могло бы
            // дать другое значение, если файл поменялся между двумя чтениями.
            db: default_db_path(&f),
            api_base: f
                .api_base
                .unwrap_or_else(|| "https://inference-api.nousresearch.com/v1".into())
                .trim_end_matches('/')
                .to_string(),
            api_key,
            embed_model: f.embed_model.unwrap_or_else(|| "baai/bge-m3".into()),
            embed_dim: f.embed_dim.unwrap_or(1024),
            distill_model: f
                .distill_model
                .unwrap_or_else(|| "deepseek/deepseek-v4.1-flash".into()),
            // Реранк — не опция: без него нечем отсекать «похоже, но не отвечает».
            // Выключить можно явно (`rerank = "off"`) — тогда решает RRF.
            rerank_model: match f.rerank.as_deref() {
                Some("off") => None,
                _ => Some(
                    f.rerank_model
                        .unwrap_or_else(|| "nvidia/llama-nemotron-rerank-vl-1b-v2:free".into()),
                ),
            },
            max_facts: f.max_facts.unwrap_or(6),
            max_tokens: f.max_tokens.unwrap_or(400),
        })
    }

    /// Ключ реранкера: тот же ключ, но у OpenRouter. Только если реранк включён.
    pub fn rerank_key(&self) -> Option<String> {
        self.rerank_model.as_ref()?;
        std::env::var("SNENS_OPENROUTER_KEY")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| key_from_file("SNENS_OPENROUTER_KEY"))
            // Последний шанс — ключ основного провайдера: если реранк отдаёт тот же
            // сервис, отдельная переменная не нужна.
            .or_else(|| Some(self.api_key.clone()))
    }
}
