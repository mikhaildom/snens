use crate::{
    cfg::Config,
    surface_runtime::{self as rt, Session},
};
use std::{fs, path::Path};

/// Заголовок, собранный из первых слов первого сообщения пользователя.
/// Используется, когда ядро/поверхность заголовок не сгенерировали. Читается по
/// уже прочитанной голове журнала: `surface_facts` берёт её один раз на все
/// нужды списка, второй `read_head_bytes` за тем же был бы лишним I/O.
pub fn title_from_head(head: &[crate::log::Rec]) -> Option<String> {
    const MAX_WORDS: usize = 8;
    const MAX_CHARS: usize = 64;
    for rec in head {
        let text = match rec {
            crate::log::Rec::User { text, .. } => text,
            crate::log::Rec::Steer { text, .. } => text,
            _ => continue,
        };
        let words: Vec<&str> = text.split_whitespace().filter(|w| !w.is_empty()).collect();
        if words.is_empty() {
            continue;
        }
        let mut out = String::new();
        for w in words.iter().take(MAX_WORDS) {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(w);
            if out.chars().count() >= MAX_CHARS {
                break;
            }
        }
        let chars: Vec<char> = out.chars().collect();
        if chars.len() > MAX_CHARS {
            out = chars[..MAX_CHARS].iter().collect::<String>();
            out.push('…');
        } else if words.len() > MAX_WORDS {
            out.push('…');
        }
        return Some(out);
    }
    None
}

/// Заголовок сессии: `Meta.title`, иначе первые слова пользователя. Пустая
/// строка не возвращается — `None` значит «заголовка нет».
pub fn title(path: &Path) -> Option<String> {
    crate::surface_facts::facts(path, false).title
}

/// Поверхность сессии: метка `Meta.surface`, иначе `local`.
///
/// Локальные сессии (`tui`, `tui-local`, `run-child`) — не «legacy»: метку они
/// не пишут, и это шум, а не признак устаревания.
pub fn surface(path: &Path) -> String {
    crate::surface_facts::facts(path, false)
        .surface
        .unwrap_or_else(|| "local".into())
}

/// Merge durable delivery sessions with JSONL transcripts. JSONL-only sessions
/// never get rewritten as a side effect of discovery.
pub fn sessions(cfg: &Config, store: &rt::Store) -> crate::R<Vec<Session>> {
    let data_dir = cfg.data_path();
    let mut sessions = store.sessions.clone();
    let dir = data_dir.join("sessions");
    if dir.is_dir() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") || !path.is_file() {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|x| x.to_str()) else {
                continue;
            };
            if !rt::valid_id(id) || sessions.contains_key(id) {
                continue;
            }
            // Родитель нужен до фактов: он решает, считать ли ходы.
            let facts_light = crate::surface_facts::facts(&path, false);
            if !facts_light.has_dialogue {
                continue;
            }
            let is_child = facts_light.parent.is_some();
            let facts = if is_child {
                crate::surface_facts::facts(&path, true)
            } else {
                facts_light
            };
            let mut session = build_session(cfg, id, &path, &facts);
            session.state = child_state_or_idle(cfg, &session, &path);
            sessions.insert(id.to_string(), session);
        }
    }
    // Сессии из store тоже фильтруются: там могли остаться огрызки, созданные
    // до того, как discovery научился их отбрасывать.
    sessions.retain(|id, _| path_from(&data_dir, id).is_some_and(|p| has_dialogue(&p)));
    for session in sessions.values_mut() {
        let Some(path) = path_from(&data_dir, &session.id) else {
            continue;
        };
        // Один проход по журналу на сессию: метка, заголовок, родитель, модель
        // и (для подагента) ходы приходят из одного кэшированного набора фактов.
        let need_progress = session.parent.is_some() || facts_parent(&path).is_some();
        let facts = crate::surface_facts::facts(&path, need_progress);
        let fresh = build_session(cfg, &session.id, &path, &facts);
        session.surface = fresh.surface;
        session.title = session.title.take().or(fresh.title);
        session.parent = fresh.parent.or(session.parent.take());
        session.model = fresh.model.or(session.model.take());
        session.updated_at = fresh.updated_at;
        // Ходы и закрытие пересчитываем всегда, а не только когда пусто:
        // подагент растёт, пока его карточка висит на экране, и «3 хода» без
        // обновления превращалось бы во враньё.
        session.turns = fresh.turns;
        session.started_at = fresh.started_at;
        session.done_at = fresh.done_at;
        session.done = fresh.done;
        session.state = if session.parent.is_some() {
            child_state_or_idle(cfg, session, &path)
        } else {
            session.state.clone()
        };
    }
    // Свежие сверху. Порядок не зависит от поверхности и от формы id
    // (`1234-567` и uuid иначе перемешивались лексикографически).
    let mut out: Vec<Session> = sessions.into_values().collect();
    out.sort_by(|a, b| {
        b.updated_at
            .unwrap_or(0)
            .cmp(&a.updated_at.unwrap_or(0))
            .then_with(|| b.id.cmp(&a.id))
    });
    Ok(out)
}

/// Родитель по журналу — без полного разбора: нужен, чтобы решить, звать ли
/// дорогой проход ради счётчика ходов.
fn facts_parent(path: &Path) -> Option<String> {
    crate::surface_facts::facts(path, false).parent
}

fn build_session(
    cfg: &Config,
    id: &str,
    path: &Path,
    facts: &crate::surface_facts::Facts,
) -> Session {
    let mut session = Session::new(id.to_string(), "local".to_string());
    session.surface = facts.surface.clone().unwrap_or_else(|| "local".into());
    if session.surface == "legacy" {
        session.surface = "local".into();
    }
    session.title = facts.title.clone();
    session.parent = facts.parent.clone();
    session.model = facts.model.clone();
    session.updated_at = mtime(path);
    if let Some(p) = &facts.progress {
        session.turns = Some(p.turns);
        session.started_at = p.started_at;
        session.done_at = p.done_at;
        session.done = p.done.clone();
    }
    let _ = cfg;
    session
}

/// Состояние подагента по журналу и pid; у обычной сессии — как было в store.
///
/// Признаки (`Done`, `Started`) берём из тех же фактов, что уже прочитаны для
/// списка: третий полный разбор журнала ради них был лишним.
fn child_state_or_idle(cfg: &Config, session: &Session, path: &Path) -> String {
    let Some(parent) = session.parent.as_deref() else {
        return session.state.clone();
    };
    let (done, started) = crate::subagent::journal_signs(path);
    child_state(&crate::subagent::state_from_signs(
        cfg,
        parent,
        &session.id,
        done,
        started,
    ))
    .into()
}

/// Модель сессии: `Meta.model`, иначе последняя запись `usage` с непустым
/// полем (появилось позже `Meta`), иначе `None` — у старых логов её нет.
pub fn model(path: &Path) -> Option<String> {
    crate::surface_facts::facts(path, false).model
}

/// Родитель сессии из `Meta` (пусто у пользовательских сессий).
pub fn parent(path: &Path) -> Option<String> {
    crate::surface_facts::facts(path, false).parent
}

/// Что видно про подагента по его журналу: сколько ходов, когда начал, когда
/// закончил и чем. Границы нужны для длительности — по ним клиент рисует «работает
/// 3 мин» без опроса.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub turns: u64,
    pub done: Option<String>,
    /// Время первой записи журнала (unix, сек) — старт подагента.
    pub started_at: Option<u64>,
    /// Время записи `Done` — конец работы.
    pub done_at: Option<u64>,
}

/// Ходы, признаки закрытия и границы работы из журнала — для карточки
/// подагента: сколько шагов прошло, чем кончилось и сколько это заняло.
/// Считается тем же проходом, что и остальные факты, и кэшируется с ними.
pub fn progress(path: &Path) -> Progress {
    crate::surface_facts::facts(path, true)
        .progress
        .unwrap_or_default()
}

/// Полный разбор журнала ради счётчика ходов. Дорого (весь файл), поэтому
/// вызывается только для подагентов и только из `surface_facts`.
pub fn progress_uncached(path: &Path) -> Progress {
    let Ok(records) = crate::log::read_file(path) else {
        return Progress::default();
    };
    let mut turns = 0u64;
    let mut done = None;
    let mut started_at = records.first().map(|r| r.ts());
    let mut done_at = None;
    for rec in &records {
        match rec {
            crate::log::Rec::Assistant { .. } => turns += 1,
            crate::log::Rec::Done { status, .. } => {
                done = Some(status.clone());
                done_at = Some(rec.ts());
            }
            _ => {}
        }
    }
    if started_at == Some(0) {
        started_at = records.iter().map(|r| r.ts()).find(|t| *t > 0);
    }
    // Перезапущенная сессия: `Done` есть, но после него снова работали —
    // последняя `Assistant` новые ходы добавляет, и закрытие уже не текущее.
    // Порядок в журнале хронологический, поэтому просто смотрим, что было
    // последним из двух событий.
    let last_done = records
        .iter()
        .rposition(|r| matches!(r, crate::log::Rec::Done { .. }));
    let last_assistant = records
        .iter()
        .rposition(|r| matches!(r, crate::log::Rec::Assistant { .. }));
    if let (Some(d), Some(a)) = (last_done, last_assistant) {
        if a > d {
            done = None;
            done_at = None;
        }
    }
    Progress {
        turns,
        done,
        started_at,
        done_at,
    }
}

/// Была ли в сессии хоть одна реплика человека или вставка в ход. Сессии без
/// них — огрызки (создали и бросили, упал запуск): в списке им делать нечего.
pub fn has_dialogue(path: &Path) -> bool {
    crate::surface_facts::facts(path, false).has_dialogue
}

/// Состояние подагента так, как его ждёт поверхность: `running` — ещё работает,
/// `idle` — закрылся нормально, `failed` — с ошибкой/по лимиту, `interrupted` —
/// процесса нет, а `Done` так и не записан.
///
/// Вход берётся из `subagent::State` — единого разбора журнала и pid: раньше
/// поверхность собирала это сама по файлу регистрации, и правило состояния
/// жило в двух местах, расходясь в мелочах.
pub fn child_state(state: &crate::subagent::State) -> &'static str {
    use crate::subagent::State;
    match state {
        State::Running => "running",
        State::Done => "idle",
        State::Failed(_) => "failed",
        State::Crashed => "interrupted",
    }
}

/// Время последней активности сессии — `mtime` её лога.
pub fn mtime(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

fn path_from(data_dir: &Path, id: &str) -> Option<std::path::PathBuf> {
    let p = data_dir.join("sessions").join(format!("{id}.jsonl"));
    p.is_file().then_some(p)
}

/// Resolve only canonical UUID JSONL paths; caller still checks file existence.
pub fn path(cfg: &Config, id: &str) -> Option<std::path::PathBuf> {
    if !rt::valid_id(id) {
        return None;
    }
    Some(cfg.sessions_path().join(format!("{id}.jsonl")))
}

pub fn load_meta_surface(path: &Path) -> Option<String> {
    crate::surface_facts::facts(path, false).surface
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!("surface-discovery-{}", uuid::Uuid::new_v4()))
    }

    /// Конфиг теста: сессии и ящик внутри временного каталога.
    fn test_cfg(root: &Path) -> Config {
        Config {
            sessions_dir: root.join("sessions").display().to_string(),
            mail_dir: root.join("mail").display().to_string(),
            ..Config::default()
        }
    }
    #[test]
    fn finds_jsonl_without_changing_bytes_and_ignores_non_uuid() {
        let root = temp();
        let dir = root.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let file = dir.join(format!("{id}.jsonl"));
        fs::write(
            &file,
            "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\"}\n{\"kind\":\"user\",\"ts\":2,\"text\":\"hi\",\"source\":\"surface\"}\n",
        )
        .unwrap();
        fs::write(dir.join("not-a-session.jsonl"), "bad\n").unwrap();
        let before = fs::read(&file).unwrap();
        let found = sessions(&test_cfg(&root), &rt::Store::default()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, id);
        assert_eq!(found[0].surface, "local");
        assert_eq!(before, fs::read(&file).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn title_falls_back_to_first_user_words() {
        let root = temp();
        let dir = root.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let file = dir.join(format!("{id}.jsonl"));
        fs::write(
            &file,
            concat!(
                "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\"}\n",
                "{\"kind\":\"user\",\"ts\":2,\"text\":\"хочу веб-интерфейс snens, из которого потом сделать десктопное приложение и жить долго\",\"source\":\"surface\"}\n"
            ),
        )
        .unwrap();
        let found = sessions(&test_cfg(&root), &rt::Store::default()).unwrap();
        let t = found[0].title.clone().unwrap();
        assert!(t.starts_with("хочу веб-интерфейс snens,"), "{t}");
        assert!(t.ends_with('…'), "{t}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sessions_sort_by_mtime_independently_of_surface_and_id_shape() {
        let root = temp();
        let dir = root.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        let uuid = uuid::Uuid::new_v4().to_string();
        let old = dir.join(format!("{uuid}.jsonl"));
        fs::write(
            &old,
            "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\"}\n{\"kind\":\"user\",\"ts\":2,\"text\":\"hi\",\"source\":\"surface\"}\n",
        )
        .unwrap();
        // uuid лексикографически "больше" номера вида 1234-567, но он старее.
        let stamp = dir.join("1790000000-123.jsonl");
        fs::write(
            &stamp,
            "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\"}\n{\"kind\":\"user\",\"ts\":2,\"text\":\"hi\",\"source\":\"surface\"}\n",
        )
        .unwrap();
        let newer = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        let f = fs::OpenOptions::new().write(true).open(&stamp).unwrap();
        f.set_modified(newer).unwrap();

        let found = sessions(&test_cfg(&root), &rt::Store::default()).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].id, "1790000000-123", "свежая — первой");
        assert!(found[0].updated_at.unwrap() > found[1].updated_at.unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    /// Состояние подагента — по двум источникам: журнал говорит, чем кончилось,
    /// pid — жив ли процесс. Ни один из них не решает в одиночку: без `Done`
    /// живой процесс значит «работает», а мёртвый — «сняли».
    #[test]
    fn child_state_maps_agent_state() {
        use crate::subagent::State;
        assert_eq!(child_state(&State::Running), "running");
        assert_eq!(child_state(&State::Done), "idle");
        assert_eq!(child_state(&State::Failed("limit".into())), "failed");
        assert_eq!(child_state(&State::Crashed), "interrupted");
    }

    #[test]
    fn session_without_dialogue_is_not_listed() {
        let root = temp();
        let dir = root.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        let created = uuid::Uuid::new_v4().to_string();
        let used = uuid::Uuid::new_v4().to_string();
        fs::write(
            dir.join(format!("{created}.jsonl")),
            "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\"}\n{\"kind\":\"system\",\"ts\":2,\"text\":\"prompt\"}\n",
        )
        .unwrap();
        fs::write(
            dir.join(format!("{used}.jsonl")),
            "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\"}\n{\"kind\":\"user\",\"ts\":2,\"text\":\"hi\",\"source\":\"surface\"}\n",
        )
        .unwrap();
        let found = sessions(&test_cfg(&root), &rt::Store::default()).unwrap();
        assert_eq!(found.len(), 1, "пустая сессия в списке не нужна");
        assert_eq!(found[0].id, used);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parent_and_model_are_read_for_subagent_sessions() {
        let root = temp();
        let dir = root.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let file = dir.join(format!("{id}.jsonl"));
        fs::write(
            &file,
            concat!(
                "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\",\"parent\":\"1790000000-1\"}\n",
                "{\"kind\":\"user\",\"ts\":2,\"text\":\"задача подагента\",\"source\":\"surface\"}\n",
                "{\"kind\":\"assistant\",\"ts\":3,\"text\":\"ok\",\"tool_calls\":[]}\n",
                "{\"kind\":\"usage\",\"ts\":4,\"model\":\"openai/gpt-6-luna\",\"in_tok\":10,\"cached_tok\":8,\"out_tok\":2,\"cost_micro\":7,\"reason_tok\":0,\"encrypted_reasoning\":false,\"upstream_id\":\"\",\"fingerprint\":\"\",\"finish_reason\":\"stop\",\"est_tok\":10}\n"
            ),
        )
        .unwrap();
        let found = sessions(&test_cfg(&root), &rt::Store::default()).unwrap();
        assert_eq!(found[0].parent.as_deref(), Some("1790000000-1"));
        assert_eq!(found[0].model.as_deref(), Some("openai/gpt-6-luna"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_meta_title_wins_over_transcript() {
        let root = temp();
        let dir = root.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let file = dir.join(format!("{id}.jsonl"));
        fs::write(
            &file,
            concat!(
                "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\",\"title\":\"мок веб-клиента\"}\n",
                "{\"kind\":\"user\",\"ts\":2,\"text\":\"совсем другой текст\",\"source\":\"surface\"}\n"
            ),
        )
        .unwrap();
        let found = sessions(&test_cfg(&root), &rt::Store::default()).unwrap();
        assert_eq!(found[0].title.as_deref(), Some("мок веб-клиента"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn progress_counts_turns_and_sees_done() {
        let dir = std::env::temp_dir().join(format!("snens-progress-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("k.jsonl");
        let records = [
            r#"{"kind":"meta","ts":1,"model":"m","parent":"p"}"#,
            r#"{"kind":"user","ts":2,"text":"q","source":"surface"}"#,
            r#"{"kind":"assistant","ts":3,"text":"a","tool_calls":[]}"#,
            r#"{"kind":"assistant","ts":4,"text":"b","tool_calls":[]}"#,
        ];
        fs::write(&path, records.join("\n")).unwrap();
        // Открытая сессия: ходы считаются, закрытия нет. Старт — первая запись
        // журнала: по нему клиент считает, сколько подагент уже работает.
        let open = progress(&path);
        assert_eq!((open.turns, open.done.as_deref()), (2, None));
        assert_eq!(open.started_at, Some(1));
        assert_eq!(open.done_at, None);
        // После `Done` сессия закрыта — это и есть «подагент закончил».
        let mut text = records.join("\n");
        text.push_str("\n{\"kind\":\"done\",\"ts\":5,\"status\":\"ok\",\"turns\":2}");
        fs::write(&path, &text).unwrap();
        let closed = progress(&path);
        assert_eq!((closed.turns, closed.done.as_deref()), (2, Some("ok")));
        assert_eq!(closed.done_at, Some(5), "длительность считается по Done");
        // Новая работа после `Done` (перезапуск) снимает закрытие.
        text.push_str("\n{\"kind\":\"assistant\",\"ts\":6,\"text\":\"c\",\"tool_calls\":[]}");
        fs::write(&path, &text).unwrap();
        let resumed = progress(&path);
        assert_eq!((resumed.turns, resumed.done.as_deref()), (3, None));
        assert_eq!(resumed.done_at, None);
        fs::remove_dir_all(dir).unwrap();
    }
}

/// Одна сессия по id, собранная из журнала: нужно, когда сессия появилась
/// позже старта поверхности (подагент пишет свой лог другим процессом), а
/// `discovery::sessions` наполняет store только целиком и только при старте.
pub fn session_by_id(cfg: &Config, id: &str) -> Option<Session> {
    let path = path_from(&cfg.data_path(), id).filter(|p| has_dialogue(p))?;
    let mut session = Session::new(id.to_string(), surface(&path));
    session.title = title(&path);
    session.updated_at = mtime(&path);
    session.parent = parent(&path);
    session.model = model(&path);
    let p = progress(&path);
    session.turns = Some(p.turns);
    session.started_at = p.started_at;
    session.done_at = p.done_at;
    if let Some(parent) = session.parent.clone() {
        session.state = child_state(&crate::subagent::state_of(cfg, &parent, id)).into();
    }
    session.done = p.done;
    Some(session)
}
