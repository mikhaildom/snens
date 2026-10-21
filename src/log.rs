// Сессия = append-only JSONL. Лог неизменяем, seq = номер строки.
// Промпт — проекция лога (project.rs), а не сам лог.
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::R;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Rec {
    Meta {
        ts: u64,
        model: String,
        /// Сессия-родитель, если это подагент. Пусто у корневой сессии.
        /// Старые логи читаются как есть (serde default).
        #[serde(default)]
        parent: String,
        /// Поверхность, создавшая сессию. Локальные логи (TUI, `tui-local`,
        /// `run-child`) метки не получают — читаются как `local`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        surface: Option<String>,
        /// Заголовок сессии. Пусто — ядро/поверхность его ещё не сгенерировали,
        /// и читатель подставляет первые слова первого сообщения пользователя.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Системный промпт сессии. Собирается один раз в начале и дальше читается
    /// из лога: файлы инстанса (soul/user/memory, скиллы) правятся по ходу, и
    /// пересборка промахивалась бы мимо префикс-кеша на каждой правке.
    System { ts: u64, text: String },
    /// Транскрипт, на котором вырос лог. Ревью — не копия сессии, а хвост поверх
    /// неё: `session` — источник, `lines` — сколько его первых раскрытых записей
    /// входит сюда. Живёт второй строкой, сразу после `Meta`; при чтении
    /// разворачивается обратно в записи источника, поэтому проекция совпадает с
    /// прежней копией байт-в-байт и префикс-кеш провайдера остаётся тёплым.
    ///
    /// Поля с `default` — чтобы `{"kind":"replay"}` (маркер без ссылки, след
    /// старых экспериментов) читался как запись: префикса в таком логе нет, и
    /// это надо помнить.
    Replay {
        #[serde(default)]
        ts: u64,
        #[serde(default)]
        session: String,
        #[serde(default)]
        lines: usize,
    },
    User {
        ts: u64,
        text: String,
        source: String,
        #[serde(default)]
        parts: Vec<Part>,
    },
    Assistant {
        ts: u64,
        text: Option<String>,
        tool_calls: Vec<Call>,
        /// Рассуждение модели, если провайдер его отдал. В логе хранится целиком
        /// (это файл сессии, не только контекст), но в проекцию не уезжает —
        /// провайдеры reasoning обратно не принимают.
        #[serde(default)]
        reasoning: Option<String>,
    },
    ToolResult {
        ts: u64,
        call_id: String,
        tool: String,
        argv: Vec<String>,
        exit: i32,
        bytes: usize,
        ms: u64,
        body: Body,
        /// Вложения, которые тул положил в канал (`SNENS_ATTACH_DIR`). Пусто у
        /// текстовых тулов; старые логи читаются как есть (serde default).
        #[serde(default)]
        parts: Vec<Part>,
    },
    Usage {
        ts: u64,
        /// Модель, которой шёл этот ход. Пусто у логов, записанных до того, как
        /// поле появилось: восстановить её из старого журнала нечем.
        #[serde(default)]
        model: String,
        in_tok: u64,
        cached_tok: u64,
        out_tok: u64,
        cost_micro: u64,
        /// Идентификатор ответа провайдера (`gen-…`) и отпечаток бэкенда, если
        /// были. Старые логи читаются как есть (serde default). Нужны, чтобы
        /// отличить наш промах кеша от переключения апстрима роутером.
        #[serde(default)]
        upstream_id: String,
        #[serde(default)]
        fingerprint: String,
        /// `finish_reason` этого хода (`stop`/`tool_calls`/`length`/`content_filter`).
        /// Старые логи читаются как есть. Нужен, чтобы «модель закончила» отличалось
        /// от «ответ оборван лимитом токенов» при разборе сессии.
        #[serde(default)]
        finish_reason: String,
        /// Токены рассуждения по счётчику провайдера. У моделей с зашифрованным
        /// reasoning это весь видимый след настоящей цены хода: текста рассуждения
        /// у нас нет, а `out_tok` показывает только короткий ответ. Старые логи
        /// читаются как есть.
        #[serde(default)]
        reason_tok: u64,
        /// Провайдер закрыл рассуждение: прислал блоб вместо текста. Старые логи
        /// читаются как есть.
        #[serde(default)]
        encrypted_reasoning: bool,
        /// Наша оценка размера той же проекции, что ушла в этот ход. Нужна,
        /// чтобы откалибровать порог по токенам провайдера: отношение
        /// `in_tok / est_tok` и есть настоящий множитель токенизатора модели,
        /// а не догадка `len/3`. Старые логи читаются как есть.
        #[serde(default)]
        est_tok: u64,
    },
    /// Что проекция сделала с контекстом. Лог остаётся неизменным: это журнал
    /// действий над ним, а не новая версия истории.
    ///
    /// `cleared` — карта `call_id -> заглушка`: какие результаты тулов уборка
    /// вырезала и чем их заменила. Проекция применяет её поверх готовых
    /// сообщений, поэтому элизия переживает resume и не дублируется.
    Ctx {
        ts: u64,
        action: String,
        elided: usize,
        freed_bytes: usize,
        #[serde(default)]
        cleared: BTreeMap<String, String>,
    },
    /// Справка из долгой памяти, впрыснутая перед ходом. В логе, а не только на
    /// проводе: проекция обязана собираться байт-в-байт одинаково и после
    /// рестарта, а resume не должен впрыскивать её второй раз.
    Recall { ts: u64, text: String },
    /// Accepted working-context projection. Original events remain immutable.
    ContextNotice { ts: u64, text: String },
    ContextRevision {
        ts: u64,
        anchor: usize,
        digest: String,
        messages: Vec<serde_json::Value>,
    },
    /// Сообщение от другой сессии (родитель, ребёнок, человек через steer),
    /// впрыснутое на границе хода. В проекции едет ролью `user` с пометкой
    /// отправителя: это чужое слово, а не ввод этой сессии.
    Steer {
        ts: u64,
        from: String,
        /// `steer` | `ask` | `report` — что это за слово. Имя поля не `kind`:
        /// `kind` занято тегом самой записи.
        via: String,
        text: String,
    },
    /// Терминатор сессии: пишет харнесс, а не модель, на всех путях выхода цикла.
    /// По нему родитель узнаёт, что ребёнок закончил, и с каким исходом —
    /// даже если модель «забыла» отчитаться.
    Done { ts: u64, status: String, turns: u32 },
    /// Отметка запуска саморевью в журнале самой сессии. Служебная строка: в
    /// проекцию промпта не уезжает (это факт о харнессе, а не реплика модели;
    /// вдобавок её токены платились бы в каждом следующем ходу и сдвигали бы
    /// префикс-кеш), но видна в транскрипте поверхностью — роль `review`.
    ///
    /// Зачем в логе: следующее ревью берёт эту точку за начало своей зоны
    /// ответственности — `turns` ходов было в момент запуска, всё, что раньше,
    /// разобрано прошлым прогоном. `text` — короткая строка для показа
    /// человеку; смысл границы держится на числах, а не на тексте.
    ///
    /// Новый вариант, а не расширение `Done`: старые логи читаются как есть
    /// (неизвестный `kind` в них просто не встречается), а `Done` — терминатор
    /// сессии, и его семантику мешать с отметкой ревью нельзя.
    Review {
        ts: u64,
        /// Идентификатор запуска: `<сессия>-review`, он же имя лога ревью.
        review_id: String,
        /// Сколько ходов (ответов ассистента) было в сессии на момент запуска.
        #[serde(default)]
        turns: u64,
        /// Строка для транскрипта («саморевью · 12 ходов»). `default` — чтобы
        /// запись без текста читалась, а не роняла лог.
        #[serde(default)]
        text: String,
    },
}

/// Текст отметки ревью для транскрипта. Одна строка, ≤80 символов: её рисует
/// клиент тонким серым шрифтом, как справку памяти (`role: "recall"`).
pub fn review_line(turns: u64) -> String {
    format!("саморевью · {turns} ходов")
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Call {
    pub id: String,
    pub name: String,
    /// Аргументы как есть, строкой JSON — так их и отдаёт модель.
    pub args: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Body {
    Inline { text: String },
}

/// Вложение сообщения: файл в хранилище инстанса плюс то, что нужно, чтобы
/// собрать часть контента для провайдера. Вид (`kind`) — нейтральный, а не
/// провайдерский: мапинг в конкретный формат живёт в проекции.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Part {
    /// `image` | `audio` | `video` | `file` — расширяется без ломки старых логов.
    pub kind: String,
    /// Абсолютный путь в каталоге сессии: файл неизменяем, проекция детерминирована.
    pub path: String,
    pub mime: String,
    pub bytes: usize,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

pub struct Session {
    id: String,
    path: PathBuf,
    file: Mutex<File>,
    lines: AtomicU64,
    /// Сессия-родитель, если это подагент; читается из `Meta` при открытии.
    parent: String,
    /// Каталог, где искать источник `Replay` (он лежит рядом с этим логом).
    dir: PathBuf,
}

impl Session {
    pub fn open(dir: &Path, id: &str) -> R<Session> {
        Session::open_surface(dir, id, None)
    }

    /// Открыть новую сессию с меткой поверхности. На существующем логе метка
    /// не меняется: это метаданные создания, а не resume.
    pub fn open_surface(dir: &Path, id: &str, surface: Option<&str>) -> R<Session> {
        Session::open_child_surface(dir, id, "", surface)
    }

    /// Открыть сессию с указанием родителя (для подагента). Родитель пишется в
    /// `Meta` один раз при создании лога и больше не меняется.
    pub fn open_child(dir: &Path, id: &str, parent: &str) -> R<Session> {
        Session::open_child_surface(dir, id, parent, None)
    }

    pub fn open_child_surface(
        dir: &Path,
        id: &str,
        parent: &str,
        surface: Option<&str>,
    ) -> R<Session> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{id}.jsonl"));
        let exists = path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)?;
        file.try_lock_exclusive().map_err(|e| {
            format!("session {id} is already open for writing (possibly by surface API); stop that writer before resuming: {e}")
        })?;
        recover_tail(&mut file, &path)?;
        let lines = AtomicU64::new(count_lines(&path)?.max(1));
        let dir = dir.to_path_buf();
        // Родитель неизменяем и живёт в `Meta`: при открытии существующего лога
        // берём его оттуда, а не из аргумента (resume подагента не должен терять связь).
        let parent = if exists {
            read_file(&path)
                .ok()
                .and_then(|rs| {
                    rs.into_iter().find_map(|r| match r {
                        Rec::Meta { parent, .. } => Some(parent),
                        _ => None,
                    })
                })
                .unwrap_or_default()
        } else {
            parent.to_string()
        };
        if !exists {
            let s = Session {
                id: id.to_string(),
                path,
                file: Mutex::new(file),
                lines,
                parent: parent.clone(),
                dir,
            };
            s.append(Rec::Meta {
                ts: now(),
                model: String::new(),
                parent,
                surface: surface.map(str::to_owned),
                title: None,
            })?;
            return Ok(s);
        }
        let s = Session {
            id: id.to_string(),
            path,
            file: Mutex::new(file),
            lines,
            parent,
            dir,
        };
        Ok(s)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn parent(&self) -> &str {
        &self.parent
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Дописать запись и сбросить на диск. Append + fsync на границе записи —
    /// вся история восстановления; недописанный хвост отбрасывается при чтении.
    pub fn append(&self, rec: Rec) -> R<u64> {
        let line = serde_json::to_string(&rec)?;
        let mut f = self.file.lock().map_err(|_| "session log lock poisoned")?;
        writeln!(f, "{line}")?;
        f.sync_data()?;
        let n = self.lines.fetch_add(1, Ordering::Relaxed);
        Ok(n)
    }

    pub fn read(&self) -> R<Vec<Rec>> {
        let raw = read_file(&self.path)?;
        Ok(expand(&raw, &self.dir))
    }

    /// Записи этого лога без раскрытия префикса `Replay`. Нужно там, где смысл
    /// имеет именно хвост (например длина хвоста при переносе рамки ревью).
    pub fn read_raw(&self) -> R<Vec<Rec>> {
        read_file(&self.path)
    }

    /// Перевести уже раскрытые записи обратно в ссылку на источник. `lines` —
    /// сколько первых раскрытых записей ссылка покрывает: файл становится
    /// `Meta` + `Replay{.., lines}` + собственные записи после ссылки.
    /// `keep_tail = false` выбрасывает и их — так же, как прежде делал `cp`
    /// при новом прогоне ревью.
    ///
    /// Переписывается только сам лог, атомарно (tmp + rename): читатели видят
    /// либо старый файл целиком, либо новый.
    pub fn rewrite_with_replay(&self, session: &str, lines: usize, keep_tail: bool) -> R<()> {
        let raw = self.read_raw()?;
        // Шапка (`Meta`) и прежняя ссылка пересобираются заново; собственный
        // хвост — это всё остальное, и он не зависит от старого префикса.
        let rest: Vec<Rec> = if keep_tail {
            let mut rest: Vec<Rec> = raw.iter().skip(1).cloned().collect();
            if matches!(rest.first(), Some(Rec::Replay { .. })) {
                rest.remove(0);
            }
            rest
        } else {
            Vec::new()
        };
        let keep = rest.len();        let mut out = Vec::with_capacity(keep + 2);
        match raw.first() {
            Some(meta @ Rec::Meta { .. }) => out.push(meta.clone()),
            _ => out.push(Rec::Meta {
                ts: now(),
                model: String::new(),
                parent: self.parent.clone(),
                surface: None,
                title: None,
            }),
        }
        out.push(Rec::Replay {
            ts: now(),
            session: session.to_string(),
            lines,
        });
        out.extend(rest);
        let tmp = self.path.with_extension("jsonl.tmp");
        let mut text = String::new();
        for rec in &out {
            text.push_str(&serde_json::to_string(rec)?);
            text.push('\n');
        }
        std::fs::write(&tmp, text)?;
        // Проверка — на содержимом, которое собираемся положить, до подмены: после
        // rename читать лог по этому пути уже нельзя (а раскрытие сосёт источник
        // целиком). Атомарность — `rename`, а не запись на место.
        let expanded = expand(&out, &self.dir);
        if expanded.len() != lines + keep {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!(
                "{session}: ссылка на {} записей не сходится (раскрылось {}) — лог не тронут",
                lines,
                expanded.len().saturating_sub(keep)
            )
            .into());
        }
        std::fs::rename(&tmp, &self.path)?;
        // Дескриптор указывает на старый (уже отвязанный) инод: дописывать через
        // него — значит писать в никуда. Переоткрываем сами, пока держим лок.
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&self.path)?;
        file.try_lock_exclusive()?;
        *self.file.lock().map_err(|_| "session log lock poisoned")? = file;
        self.lines.store(out.len() as u64, Ordering::Relaxed);
        Ok(())
    }
}

/// With the writer lock held, validate complete records and repair only an
/// incomplete final line. A valid unterminated JSON record is preserved.
fn recover_tail(file: &mut File, path: &Path) -> R<()> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let complete_end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);

    for (i, line) in bytes[..complete_end].split(|b| *b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        serde_json::from_slice::<Rec>(line).map_err(|e| {
            format!(
                "corrupt session log {} at complete line {}: {e}",
                path.display(),
                i + 1
            )
        })?;
    }

    if complete_end < bytes.len() {
        let tail = &bytes[complete_end..];
        if serde_json::from_slice::<Rec>(tail).is_ok() {
            file.seek(SeekFrom::End(0))?;
            file.write_all(b"\n")?;
        } else {
            file.set_len(complete_end as u64)?;
        }
        file.sync_data()?;
    }
    Ok(())
}

/// Первые `n` записей файла без чтения остального. Нужно там, где всё решается
/// по шапке лога (`Meta`, `Replay`), а сам лог бывает в мегабайты.
pub fn read_head(path: &Path, n: usize) -> R<Vec<Rec>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for line in BufReader::new(File::open(path)?).lines().take(n) {
        let line = line?;
        if let Ok(r) = serde_json::from_str::<Rec>(&line) {
            out.push(r);
        }
    }
    Ok(out)
}

/// Ссылка на источник транскрипта: `(сессия, сколько её раскрытых записей входит
/// в этот лог)`. Живёт второй строкой, сразу после `Meta`.
pub fn replay_link(recs: &[Rec]) -> Option<(&str, usize)> {
    match recs.get(1)? {
        Rec::Replay { session, lines, .. } if !session.is_empty() => Some((session, *lines)),
        _ => None,
    }
}

fn replay_source(dir: &Path, session: &str, depth: usize) -> Option<Vec<Rec>> {
    let path = dir.join(format!("{session}.jsonl"));
    if !path.is_file() {
        return None;
    }
    let raw = read_file(&path).ok()?;
    Some(expand_depth(&raw, dir, depth))
}

/// Раскрыть `Replay` в записи источника: на выходе те же записи, что лежали бы
/// в полной копии лога (маркер в них не входит — он адрес, а не событие).
/// Ссылка не разрешилась (источник удалён, кольцо ссылок) — отдаём всё, что есть,
/// кроме маркера, и говорим в stderr: потерянный префикс — это потерянный
/// транскрипт ревью, и молчать об этом нельзя.
pub fn expand(recs: &[Rec], dir: &Path) -> Vec<Rec> {
    expand_depth(recs, dir, 0)
}

fn expand_depth(recs: &[Rec], dir: &Path, depth: usize) -> Vec<Rec> {
    const MAX_DEPTH: usize = 8;
    let Some((session, lines)) = replay_link(recs) else {
        return recs.to_vec();
    };
    let src = if depth >= MAX_DEPTH {
        eprintln!("[log] префикс `{session}` вложен глубже {MAX_DEPTH} — читаю лог без него");
        None
    } else {
        replay_source(dir, session, depth + 1)
    };
    let Some(src) = src else {
        eprintln!("[log] префикс `{session}` не раскрыт — читаю лог без него");
        // Маркер без префикса не оставляем: в проекции он дал бы пометку «выше
        // транскрипт», которого нет. Указатель остаётся только в файле.
        let mut out = vec![recs[0].clone()];
        out.extend(recs.iter().skip(2).cloned());
        return out;
    };
    // Префикс берётся из источника целиком, вместе с его `Meta`: digest ревизии
    // контекста считается по первым записям, и любая подмена шапки его ломает.
    let mut out: Vec<Rec> = src.into_iter().take(lines).collect();
    // Сама ссылка в раскрытый лог не идёт: она — адрес префикса, а не событие.
    // На провод (и в проекцию) едет ровно то, что лежало бы в старой копии лога.
    out.extend(recs.iter().skip(2).cloned());
    out
}

pub fn read_file(path: &Path) -> R<Vec<Rec>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let f = File::open(path)?;
    let mut out = Vec::new();
    for (i, line) in BufReader::new(f).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Rec>(&line) {
            Ok(r) => out.push(r),
            // Битая последняя строка — оборванная запись, а не порча данных.
            Err(e) => {
                if i as u64 + 1 == count_lines(path)? {
                    continue;
                }
                eprintln!("[log] строка {} пропущена: {e}", i + 1);
            }
        }
    }
    Ok(out)
}

/// Прочитать только голову лога: до `max_bytes` или до первой нераскрытой
/// ошибки. Нужно discovery для заголовка сессии — читать 300-килобайтный лог
/// ради первых двух записей на каждый список сессий слишком дорого.
pub fn read_head_bytes(path: &Path, max_bytes: u64) -> R<Vec<Rec>> {
    use std::io::Read;
    if !path.exists() {
        return Ok(vec![]);
    }
    let mut buf = Vec::new();
    File::open(path)?
        .take(max_bytes)
        .read_to_end(&mut buf)?;
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset < buf.len() {
        let Some(nl) = buf[offset..].iter().position(|b| *b == b'\n') else {
            // Неполная последняя строка головы — не ошибка, просто не читаем её.
            break;
        };
        let line = &buf[offset..offset + nl];
        offset += nl + 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Rec>(line) {
            Ok(r) => out.push(r),
            Err(_) => break,
        }
    }
    Ok(out)
}

fn count_lines(path: &Path) -> R<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let f = File::open(path)?;
    Ok(BufReader::new(f).lines().count() as u64)
}

pub fn latest(dir: &Path) -> R<String> {
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let mt = entry.metadata().and_then(|m| m.modified())?;
        let id = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        if best.as_ref().map(|(t, _)| mt > *t).unwrap_or(true) {
            best = Some((mt, id));
        }
    }
    best.map(|(_, id)| id)
        .ok_or_else(|| format!("сессий нет в {}", dir.display()).into())
}


pub fn list(dir: &Path) -> R<Vec<String>> {
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            if let Some(s) = p.file_stem().and_then(|s| s.to_str()) {
                ids.push(s.to_string());
            }
        }
    }
    ids.sort();
    Ok(ids)
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Короткий id сессии: время + pid, читаемо и без коллизий на практике.
pub fn new_id() -> String {
    format!("{}-{}", now(), std::process::id() % 100000)
}

#[cfg(test)]
mod writer_safety_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("snens-log-test-{}-{nonce}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn user(text: &str) -> Rec {
        Rec::User {
            ts: 1,
            text: text.to_string(),
            source: "test".to_string(),
            parts: vec![],
        }
    }

    #[test]
    fn subprocess_writer_observes_lifetime_lock() {
        let Some(dir) = std::env::var_os("SNENS_LOG_LOCK_TEST_DIR") else {
            return;
        };
        let error = Session::open(Path::new(&dir), "lock").err().unwrap();
        assert!(error.to_string().contains("already open for writing"));
    }

    #[test]
    fn writer_lock_is_exclusive_read_only_access_works_and_drop_releases() {
        let dir = TempDir::new();
        let session = Session::open(&dir.0, "lock").unwrap();
        let path = session.path().to_path_buf();
        let second = Session::open(&dir.0, "lock");
        assert!(second.err().unwrap().to_string().contains("surface API"));
        assert_eq!(read_file(&path).unwrap().len(), 1);
        assert_eq!(list(&dir.0).unwrap(), vec!["lock"]);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("log::writer_safety_tests::subprocess_writer_observes_lifetime_lock")
            .arg("--nocapture")
            .env("SNENS_LOG_LOCK_TEST_DIR", &dir.0)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "subprocess failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        drop(session);
        let reopened = Session::open(&dir.0, "lock").unwrap();
        reopened.append(user("after release")).unwrap();
        assert_eq!(reopened.read().unwrap().len(), 2);
    }

    #[test]
    fn valid_unterminated_tail_is_preserved_and_separated_before_append() {
        let dir = TempDir::new();
        let path = dir.0.join("tail.jsonl");
        let rec = serde_json::to_vec(&user("preserve me")).unwrap();
        std::fs::write(&path, &rec).unwrap();
        let session = Session::open(&dir.0, "tail").unwrap();
        session.append(user("new record")).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.windows(2).any(|w| w == b"}\n"));
        let records = read_file(&path).unwrap();
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn malformed_unterminated_tail_is_truncated_before_append() {
        let dir = TempDir::new();
        let path = dir.0.join("tail.jsonl");
        let valid = serde_json::to_vec(&user("keep")).unwrap();
        let mut contents = valid;
        contents.extend_from_slice(b"\n{\"kind\":");
        std::fs::write(&path, contents).unwrap();
        let session = Session::open(&dir.0, "tail").unwrap();
        session.append(user("next")).unwrap();
        let records = read_file(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert!(matches!(&records[0], Rec::User { text, .. } if text == "keep"));
    }

    #[test]
    fn surface_marker_is_written_only_for_new_logs_and_legacy_meta_defaults() {
        let dir = TempDir::new();
        let session = Session::open_surface(&dir.0, "api", Some("telegram")).unwrap();
        let records = session.read().unwrap();
        assert!(
            matches!(&records[0], Rec::Meta { surface: Some(surface), .. } if surface == "telegram")
        );
        drop(session);

        let legacy = dir.0.join("legacy.jsonl");
        std::fs::write(
            &legacy,
            b"{\"kind\":\"meta\",\"ts\":1,\"model\":\"\",\"parent\":\"\"}\n",
        )
        .unwrap();
        let session = Session::open_surface(&dir.0, "legacy", Some("telegram")).unwrap();
        assert!(matches!(
            &session.read().unwrap()[0],
            Rec::Meta { surface: None, .. }
        ));
    }

    #[test]
    fn corruption_in_complete_middle_record_rejects_writer_open() {
        let dir = TempDir::new();
        let path = dir.0.join("bad.jsonl");
        std::fs::write(&path, b"{}\nnot-json\n{}\n").unwrap();
        assert!(Session::open(&dir.0, "bad").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{}\nnot-json\n{}\n");
    }

    /// Ссылка на префикс: файл лога ревью содержит `Meta` + `Replay` + хвост, но
    /// читается ровно как прежняя копия. Это и есть контракт старого формата.
    #[test]
    fn replay_link_reads_as_the_former_copy() {
        let dir = TempDir::new();
        let base = Session::open(&dir.0, "base").unwrap();
        let full = vec![
            user("первый ход"),
            Rec::Assistant {
                ts: 1,
                text: Some("ответ".into()),
                tool_calls: vec![],
                reasoning: Some("думал".into()),
            },
        ];
        for rec in full {
            base.append(rec).unwrap();
        }
        let n = base.read().unwrap().len();
        drop(base);

        let review = Session::open(&dir.0, "base-review").unwrap();
        review.rewrite_with_replay("base", n, true).unwrap();
        drop(review);

        let on_disk = std::fs::read_to_string(dir.0.join("base-review.jsonl")).unwrap();
        assert_eq!(on_disk.lines().count(), 2, "префикс остался в файле: {on_disk}");
        assert!(
            read_file(&dir.0.join("base-review.jsonl")).unwrap()
                .iter()
                .any(|r| matches!(r, Rec::Replay { .. })),
            "ссылка не записана"
        );

        let reopened = Session::open(&dir.0, "base-review").unwrap();
        let expanded = reopened.read().unwrap();
        let copy = read_file(&dir.0.join("base.jsonl")).unwrap();
        assert_eq!(
            serde_json::to_string(&expanded).unwrap(),
            serde_json::to_string(&copy).unwrap(),
            "раскрытый лог не совпал с копией до байта"
        );
    }

    /// Хвост — своя работа поверх чужого транскрипта; источник мог вырасти, но
    /// длина ссылки не меняется: иначе префикс поплывёт.
    #[test]
    fn replay_tail_appends_after_prefix_and_survives_source_growth() {
        let dir = TempDir::new();
        let base = Session::open(&dir.0, "base").unwrap();
        base.append(user("общий ход")).unwrap();
        let n = base.read().unwrap().len();
        drop(base);

        let review = Session::open(&dir.0, "base-review").unwrap();
        review.rewrite_with_replay("base", n, true).unwrap();
        review.append(user("рамка ревью")).unwrap();
        drop(review);

        // Сессия дописала свой ход уже после старта ревью.
        let base = Session::open(&dir.0, "base").unwrap();
        base.append(user("поздний ход сессии")).unwrap();
        drop(base);

        let expanded = Session::open(&dir.0, "base-review").unwrap().read().unwrap();
        let texts: Vec<&str> = expanded
            .iter()
            .filter_map(|r| match r {
                Rec::User { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["общий ход", "рамка ревью"], "поздний ход утёк в префикс");
    }

    /// Ссылка, которая не разрешается (источник удалён), не должна раскрываться в
    /// пустоту молча: лог читается как есть — хвост лучше, чем ничего.
    #[test]
    fn broken_replay_link_keeps_the_tail_without_prefix() {
        let dir = TempDir::new();
        let meta = "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\",\"parent\":\"\"}";
        let replay = "{\"kind\":\"replay\",\"ts\":2,\"session\":\"gone\",\"lines\":9}";
        let tail = "{\"kind\":\"user\",\"ts\":3,\"text\":\"хвост\",\"source\":\"review\",\"parts\":[]}";
        std::fs::write(
            dir.0.join("orphan-review.jsonl"),
            format!("{meta}\n{replay}\n{tail}\n"),
        )
        .unwrap();
        let got = Session::open(&dir.0, "orphan-review").unwrap().read().unwrap();
        assert_eq!(got.len(), 2, "ссылка растворилась, хвост должен остаться");
        assert!(matches!(&got[1], Rec::User { text, .. } if text == "хвост"));
    }

    /// Ссылка на себя или кольцо не должны уводить чтение в бесконечность.
    #[test]
    fn replay_cycle_is_bounded() {
        let dir = TempDir::new();
        let head = "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\",\"parent\":\"\"}\n";
        std::fs::write(
            dir.0.join("a.jsonl"),
            [head, "{\"kind\":\"replay\",\"ts\":2,\"session\":\"b\",\"lines\":9}\n"].concat(),
        )
        .unwrap();
        std::fs::write(
            dir.0.join("b.jsonl"),
            [head, "{\"kind\":\"replay\",\"ts\":2,\"session\":\"a\",\"lines\":9}\n"].concat(),
        )
        .unwrap();
        let got = Session::open(&dir.0, "a").unwrap().read().unwrap();
        assert!(!got.is_empty(), "кольцо не должно обнулять лог");
    }

    /// Отметка ревью читается в двух видах и в проекцию не едет.
    #[test]
    fn review_marker_reads_back_and_stays_out_of_the_projection() {
        let dir = TempDir::new();
        let session = Session::open(&dir.0, "s").unwrap();
        session.append(user("работа")).unwrap();
        session
            .append(Rec::Review {
                ts: 5,
                review_id: "s-review".into(),
                turns: 12,
                text: review_line(12),
            })
            .unwrap();
        let recs = session.read().unwrap();
        assert!(matches!(recs.last(), Some(Rec::Review { turns, .. }) if *turns == 12));
        let cfg = crate::cfg::Config::default();
        let wire = serde_json::to_string(&crate::project::raw_msgs(&recs, &cfg)).unwrap();
        assert!(!wire.contains("саморевью"), "маркер уехал в промпт: {wire}");
        drop(session);

        // Лог, записанный без `turns`/`text` (чужая или будущая запись), читается.
        std::fs::write(
            dir.0.join("old.jsonl"),
            concat!(
                "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\",\"parent\":\"\"}\n",
                "{\"kind\":\"review\",\"ts\":2,\"review_id\":\"old-review\"}\n"
            ),
        )
        .unwrap();
        let old = read_file(&dir.0.join("old.jsonl")).unwrap();
        assert!(
            matches!(&old[1], Rec::Review { turns: 0, text, review_id, .. } if text.is_empty() && review_id == "old-review")
        );
    }
}
