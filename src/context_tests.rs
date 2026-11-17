use super::*;
use crate::log::{Body, Call};

struct Fixture {
    dir: PathBuf,
    sess: Session,
    cfg: Config,
}

impl Fixture {
    fn new() -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "snens-context-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let sess = Session::open(&dir, "test").unwrap();
        sess.append(Rec::System {
            ts: 1,
            text: "original system".into(),
        })
        .unwrap();
        // Бюджет мелкий, чтобы проверка сжатия была видна на синтетике:
        // минимум — 2000 токенов независимо от процента.
        let cfg = toml::from_str::<Config>(
            "ctx_window = 40000\nmax_result_chars = 200000\ncontext_budget = 20000\n",
        )
        .unwrap();
        Self { dir, sess, cfg }
    }

    fn user(&self, text: &str) {
        self.sess
            .append(Rec::User {
                ts: 1,
                text: text.into(),
                source: "human".into(),
                parts: Vec::new(),
                ask: None,
            })
            .unwrap();
    }

    fn assistant(&self, text: &str) {
        self.sess
            .append(Rec::Assistant {
                ts: 1,
                text: Some(text.into()),
                tool_calls: Vec::new(),
                reasoning: None,
            })
            .unwrap();
    }

    fn call(&self, id: &str, name: &str, args: Value) {
        self.sess
            .append(Rec::Assistant {
                ts: 1,
                text: None,
                tool_calls: vec![Call {
                    id: id.into(),
                    name: name.into(),
                    args: args.to_string(),
                }],
                reasoning: None,
            })
            .unwrap();
    }

    fn result(&self, id: &str, text: &str) {
        self.sess
            .append(Rec::ToolResult {
                ts: 1,
                call_id: id.into(),
                tool: "run".into(),
                argv: vec![],
                exit: 0,
                bytes: text.len(),
                ms: 0,
                body: Body::Inline { text: text.into() },
                parts: vec![],
            })
            .unwrap();
    }

    fn apply(&self, m: &mut Mirror) -> String {
        m.prepare(&self.sess.read().unwrap(), &self.cfg).unwrap();
        m.apply(&self.sess, &self.cfg).unwrap().unwrap_or_default()
    }

    fn revisions(&self) -> usize {
        self.sess
            .read()
            .unwrap()
            .iter()
            .filter(|r| matches!(r, Rec::ContextRevision { .. }))
            .count()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn document(m: &Mirror) -> Document {
    serde_json::from_slice(&fs::read(m.path()).unwrap()).unwrap()
}

fn save(m: &Mirror, doc: &Document) {
    fs::write(m.path(), serde_json::to_vec_pretty(doc).unwrap()).unwrap();
}

fn note(doc: &mut Document, text: &str) {
    doc.blocks.push(context_document::Block {
        id: format!("new-{}", doc.blocks.len()),
        messages: vec![json!({"role":"user", "content":text})],
    });
}

fn block_with(doc: &Document, text: &str) -> String {
    doc.blocks
        .iter()
        .find(|b| b.messages.iter().any(|m| m["content"] == text))
        .unwrap_or_else(|| panic!("block with {text:?} not found"))
        .id
        .clone()
}

/// Блок, у которого есть сообщение с текстом, начинающимся с `prefix`: проекция
/// обрезает длинные тексты, и точное равенство на крупных вставках не работает.
fn block_starting_with(doc: &Document, prefix: &str) -> String {
    doc.blocks
        .iter()
        .find(|b| {
            b.messages.iter().any(|m| {
                m["content"]
                    .as_str()
                    .is_some_and(|text| text.starts_with(prefix))
            })
        })
        .unwrap_or_else(|| panic!("block starting with {prefix:?} not found"))
        .id
        .clone()
}

fn bulk(f: &Fixture, size: usize) {
    f.call("bulk", "run", json!({"args":["cat bulk"]}));
    f.result("bulk", &format!("BULK_PAYLOAD:{}", "x".repeat(size)));
}

/// Снять крупный результат тула — самая частая настоящая правка зеркала.
/// `false` — крупного блока в документе нет.
fn shrink_bulk(doc: &mut Document) -> bool {
    for block in &mut doc.blocks {
        for message in &mut block.messages {
            if message["role"] == "tool"
                && message["content"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("BULK_PAYLOAD"))
            {
                message["content"] = json!("short");
                return true;
            }
        }
    }
    false
}

/// Урезать большой assistant-блок до `chars` символов и перезаписать зеркало.
/// Возвращает id блока.
fn set_analysis(m: &Mirror, chars: usize) -> String {
    let mut doc = document(m);
    let block = doc
        .blocks
        .iter_mut()
        .find(|b| {
            b.messages.iter().any(|m| {
                m["content"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("ANALYSIS:"))
            })
        })
        .unwrap_or_else(|| panic!("analysis block not found"));
    let id = block.id.clone();
    block.messages[0]["content"] = json!(format!("ANALYSIS:{}", "a".repeat(chars)));
    save(m, &doc);
    id
}

/// (а) Удаление user-блока из середины диалога проходит: контекст принадлежит
/// модели, защищены только первый и последний запрос человека.
#[test]
fn mid_dialog_user_block_is_deletable() {
    let f = Fixture::new();
    f.user("first task");
    f.assistant("answer one");
    f.user("obsolete question");
    f.assistant("answer two");
    bulk(&f, 30_000);
    f.user("latest instruction");
    let mut m = Mirror::open(&f.sess, &f.cfg).unwrap();
    let mut d = document(&m);
    let stale = block_with(&d, "obsolete question");
    d.blocks.retain(|b| b.id != stale);
    assert!(shrink_bulk(&mut d));
    save(&m, &d);
    let outcome = f.apply(&mut m);
    assert!(outcome.starts_with("context applied:"), "{outcome}");
    let text = serde_json::to_string(&effective(&f.sess.read().unwrap(), &f.cfg).unwrap()).unwrap();
    assert!(
        !text.contains("obsolete question"),
        "mid user block survived"
    );
    assert!(text.contains("first task"));
    assert!(text.contains("latest instruction"));
    assert_eq!(f.revisions(), 1);
}

/// (б) Удаление первого и последнего user-блока отвергается, и отказ называет
/// id, причину и допустимое действие — одной строкой.
#[test]
fn first_and_last_user_blocks_are_protected_with_clear_diagnosis() {
    let f = Fixture::new();
    f.user("original objective");
    f.assistant("work");
    bulk(&f, 20_000);
    f.user("current instruction");
    let mut m = Mirror::open(&f.sess, &f.cfg).unwrap();
    for (target, label) in [
        ("original objective", "first"),
        ("current instruction", "last"),
    ] {
        let mut d = document(&m);
        let id = block_with(&d, target);
        d.blocks.retain(|b| b.id != id);
        save(&m, &d);
        let outcome = f.apply(&mut m);
        assert!(outcome.contains("context rejected:"), "{outcome}");
        assert!(outcome.contains(&id), "block id missing: {outcome}");
        assert!(outcome.contains(label), "policy missing: {outcome}");
        assert!(
            outcome.contains("edit or remove other blocks instead")
                || outcome.contains("must stay before"),
            "allowed action missing: {outcome}"
        );
        assert!(!outcome.contains("cannot delete blocks containing only"));
        assert_eq!(f.revisions(), 0, "rejected draft left a revision");
    }
    // Правка защищённого блока тоже отвергается с внятным текстом.
    let mut d = document(&m);
    let id = block_with(&d, "original objective");
    let block = d.blocks.iter_mut().find(|b| b.id == id).unwrap();
    block.messages[0]["content"] = json!("rewritten objective");
    save(&m, &d);
    let outcome = f.apply(&mut m);
    assert!(outcome.contains("cannot be edited"), "{outcome}");
    assert!(outcome.contains("new-"), "{outcome}");
    // Перестановка первого запроса за последний — тоже отказ.
    let mut d = document(&m);
    let first = block_with(&d, "original objective");
    let at = d.blocks.iter().position(|b| b.id == first).unwrap();
    let block = d.blocks.remove(at);
    d.blocks.push(block);
    save(&m, &d);
    let outcome = f.apply(&mut m);
    assert!(outcome.contains("was moved"), "{outcome}");
}

/// (в) Правка, которая не сжала проекцию, отвергается с числами и списком
/// крупнейших блоков; сжатие на минимум принимается и подтверждается числами.
#[test]
fn edit_without_shrink_is_rejected_with_numbers() {
    let f = Fixture::new();
    f.user("objective");
    f.assistant(&format!("ANALYSIS:{}", "a".repeat(30_000)));
    bulk(&f, 60_000);
    f.user("instruction");
    let mut m = Mirror::open(&f.sess, &f.cfg).unwrap();
    // Минимальное сжатие по дефолту: 1% от 20000 = 200, но не меньше 2000.
    assert_eq!(f.cfg.context_min_savings(), 2000);

    // Правка ради правки: те же размеры, ничего не снято.
    let mut d = document(&m);
    let id = block_with(&d, &format!("ANALYSIS:{}", "a".repeat(30_000)));
    let block = d.blocks.iter_mut().find(|b| b.id == id).unwrap();
    block.messages[0]["content"] = json!(format!("ANALYSIS:{}", "b".repeat(30_000)));
    save(&m, &d);
    let outcome = f.apply(&mut m);
    assert!(outcome.contains("context rejected:"), "{outcome}");
    assert!(outcome.contains("нужно снять не меньше"), "{outcome}");
    assert_eq!(f.revisions(), 0, "non-shrinking edit was applied");
    // Отказ механический: id крупнейшего блока и остаток до цели.
    assert!(outcome.contains("b-"), "top blocks missing: {outcome}");
    assert!(outcome.contains("Крупнейшие блоки"), "{outcome}");
    assert!(outcome.contains("токенов"), "{outcome}");

    // Настоящее сжатие принимается и сообщает before/after, снятое и цель.
    let mut d = document(&m);
    assert!(shrink_bulk(&mut d));
    save(&m, &d);
    let outcome = f.apply(&mut m);
    assert!(
        outcome.starts_with("context applied: revision "),
        "{outcome}"
    );
    assert!(outcome.contains("tokens ~"), "{outcome}");
    assert!(outcome.contains("снято "), "{outcome}");
    assert!(outcome.contains("цель "), "{outcome}");
    assert_eq!(f.revisions(), 1);
}

/// `context_min_savings` задаёт минимум явно и перебивает дефолт.
#[test]
fn explicit_min_savings_replaces_the_default() {
    let f = Fixture::new();
    f.user("objective");
    f.assistant(&format!("ANALYSIS:{}", "a".repeat(30_000)));
    f.user("instruction");
    // По дефолту минимум — 2000 токенов.
    assert_eq!(f.cfg.context_min_savings(), 2000);
    let mut cfg = f.cfg.clone();
    cfg.context_min_savings = Some(1);
    assert_eq!(cfg.context_min_savings(), 1);
    let mut m = Mirror::open(&f.sess, &cfg).unwrap();
    // Компенсируем обрезку длинных вставок в проекции: берём факт из зеркала.
    let _ = block_starting_with(&document(&m), "ANALYSIS:");
    // Сжатие ~3000 символов (≈1000 токенов): меньше дефолтных 2000, но больше
    // явного минимума — принимается.
    set_analysis(&m, 27_000);
    m.prepare(&f.sess.read().unwrap(), &cfg).unwrap();
    let outcome = m.apply(&f.sess, &cfg).unwrap().unwrap();
    assert!(outcome.starts_with("context applied:"), "{outcome}");
    // Такая же по размеру правка с дефолтным минимумом — отказ.
    set_analysis(&m, 24_000);
    m.prepare(&f.sess.read().unwrap(), &f.cfg).unwrap();
    let outcome = m.apply(&f.sess, &f.cfg).unwrap().unwrap();
    assert!(outcome.contains("нужно снять не меньше 2000"), "{outcome}");
    // `0` отключает проверку: правка без сжатия принимается.
    let mut off = f.cfg.clone();
    off.context_min_savings = Some(0);
    assert_eq!(off.context_min_savings(), 0);
    let mut m = Mirror::open(&f.sess, &off).unwrap();
    set_analysis(&m, 26_000);
    m.prepare(&f.sess.read().unwrap(), &off).unwrap();
    let outcome = m.apply(&f.sess, &off).unwrap().unwrap();
    assert!(outcome.starts_with("context applied:"), "{outcome}");
}

/// (г) Форма `content` не решает, защищён блок или нет: массив `parts` и
/// строка с тем же текстом сравниваются одинаково.
#[test]
fn content_parts_and_string_are_compared_alike() {
    let messages = vec![
        json!({"role":"system","content":"sys"}),
        json!({"role":"user","content":"start"}),
        json!({"role":"assistant","content":[{"type":"text","text":"parts answer"}]}),
        json!({"role":"user","content":[{"type":"text","text":"finish"}]}),
    ];
    let view = View::new(&messages, "r".into()).unwrap();
    // Последний запрос пришёл массивом частей и защищён так же, как строкой:
    // тот же текст строкой принимается как «не изменён».
    let mut edited = view.document.clone();
    edited.blocks[2].messages[0]["content"] = json!("finish");
    let restored = view.restore(&edited).unwrap();
    assert_eq!(restored.last().unwrap()["content"], "finish");
    // Другой текст — отказ с политикой, а не с «неизвестной ошибкой».
    let mut edited = view.document.clone();
    edited.blocks[2].messages[0]["content"] =
        json!([{"type":"text","text":"rewritten instruction"}]);
    let error = view.restore(&edited).unwrap_err().to_string();
    assert!(error.contains("cannot be edited"), "{error}");
    assert!(error.contains("last"), "{error}");
    // Assistant-текст массивом частей не защищён: его можно заменить саммари.
    let mut edited = view.document.clone();
    edited.blocks[1].messages[0]["content"] = json!("summary");
    let restored = view.restore(&edited).unwrap();
    assert_eq!(restored[2]["content"], "summary");
}

/// (д) Сирота-вызов отвергается: пара «вызов ↔ результат» атомарна.
#[test]
fn orphaned_tool_call_is_rejected() {
    let f = Fixture::new();
    f.user("start");
    bulk(&f, 40_000);
    f.user("finish");
    let mut m = Mirror::open(&f.sess, &f.cfg).unwrap();
    let mut d = document(&m);
    let at = d
        .blocks
        .iter()
        .position(|b| b.messages.iter().any(|m| m["tool_calls"].is_array()))
        .unwrap();
    d.blocks[at].messages.pop();
    save(&m, &d);
    let outcome = f.apply(&mut m);
    assert!(outcome.contains("context rejected:"), "{outcome}");
    assert!(
        outcome.contains("lack results") || outcome.contains("orphan"),
        "{outcome}"
    );
    assert_eq!(f.revisions(), 0);
    // Вся пара вызов-результат удаляется целиком — атомарность держится.
    let mut d = document(&m);
    let id = d.blocks[at].id.clone();
    d.blocks.retain(|b| b.id != id);
    save(&m, &d);
    let outcome = f.apply(&mut m);
    assert!(outcome.starts_with("context applied:"), "{outcome}");
}

/// (е) Заметка `new-*` добавляется — это рабочая записка модели.
#[test]
fn new_note_block_is_accepted() {
    let f = Fixture::new();
    f.user("start");
    bulk(&f, 30_000);
    f.user("finish");
    let mut m = Mirror::open(&f.sess, &f.cfg).unwrap();
    let mut d = document(&m);
    note(&mut d, "continuation note: decisions kept");
    assert!(shrink_bulk(&mut d));
    save(&m, &d);
    let outcome = f.apply(&mut m);
    assert!(outcome.starts_with("context applied:"), "{outcome}");
    let text = serde_json::to_string(&effective(&f.sess.read().unwrap(), &f.cfg).unwrap()).unwrap();
    assert!(text.contains("continuation note"));
    // Заметка без `new-` — неизвестный id.
    let mut d = document(&m);
    d.blocks.push(context_document::Block {
        id: "note-1".into(),
        messages: vec![json!({"role":"user","content":"x"})],
    });
    save(&m, &d);
    let outcome = f.apply(&mut m);
    assert!(outcome.contains("unknown block id"), "{outcome}");
}

/// (ж) Сбой записи журнала не оставляет активной ревизии и откатывает зеркало.
#[test]
fn failed_revision_append_leaves_no_active_revision() {
    let f = Fixture::new();
    f.user("start");
    bulk(&f, 40_000);
    f.user("finish");
    let mut m = Mirror::open(&f.sess, &f.cfg).unwrap();
    let mut d = document(&m);
    let at = d
        .blocks
        .iter()
        .position(|b| b.messages.iter().any(|m| m["role"] == "tool"))
        .unwrap();
    d.blocks[at].messages[1]["content"] = json!("short");
    save(&m, &d);
    let canonical = effective(&f.sess.read().unwrap(), &f.cfg).unwrap();
    FAIL_APPEND.store(true, Ordering::Relaxed);
    m.prepare(&f.sess.read().unwrap(), &f.cfg).unwrap();
    let error = m.apply(&f.sess, &f.cfg).unwrap_err().to_string();
    assert!(error.contains("append failed"), "{error}");
    assert_eq!(
        f.revisions(),
        0,
        "revision persisted despite append failure"
    );
    // Зеркало откатилось к каноническому: следующая подготовка даёт ту же проекцию.
    let restored = m.prepare(&f.sess.read().unwrap(), &f.cfg).unwrap();
    assert_eq!(restored, canonical);
    let document = document(&m);
    assert!(document
        .blocks
        .iter()
        .any(|b| b.messages.iter().any(|m| m["content"]
            .as_str()
            .is_some_and(|s| s.contains("BULK_PAYLOAD")))));
}

/// (з) Ступени давления взводятся по возрастанию и перевзводятся после спада.
#[test]
fn pressure_tiers_arm_and_rearm() {
    let tiers = vec![10_000, 15_000];
    let mut pressure = Pressure::armed(tiers.clone());
    // Ниже первой ступени — тишина.
    assert_eq!(pressure.observe(9_000, 20_000), None);
    // Первая ступень: следующая точка — вторая ступень либо +четверть бюджета.
    let next = pressure.observe(11_000, 20_000).expect("first tier");
    assert!(next >= 15_000, "{next}");
    // Между ступенями тишина: повторного требования на каждом ходу нет.
    assert_eq!(pressure.observe(12_000, 20_000), None);
    // Вторая ступень: порог снова сработал на 16k.
    assert!(
        pressure.observe(16_000, 20_000).is_some(),
        "tier refired early"
    );
    // После второй ступени порог растёт приростом на четверть бюджета.
    assert_eq!(pressure.observe(18_000, 20_000), None);
    // Спад ниже нижней ступени перевзводит все ступени.
    assert_eq!(pressure.observe(4_000, 20_000), None);
    assert!(
        pressure.observe(10_000, 20_000).is_some(),
        "tier not re-armed"
    );
    // Resume продолжает гистерезис, а не начинает заново.
    let mut resumed = Pressure::resumed(tiers.clone(), Some(30_000));
    assert_eq!(resumed.observe(20_000, 20_000), None);
    assert!(resumed.observe(30_000, 20_000).is_some());
    // Пустой список ступеней — поведение «одна ступень по бюджету».
    let mut single = Pressure::armed(vec![20_000]);
    assert!(single.observe(20_000, 20_000).is_some());
    assert_eq!(single.observe(21_000, 20_000), None);
}

/// Повтор `ctx commit` без правки обязан говорить, что ревизия уже принята:
/// голое «unchanged» модель читает как «не прошло» и повторяет вызов.
#[test]
fn repeated_commit_reports_already_applied_revision() {
    let f = Fixture::new();
    f.user("start");
    bulk(&f, 30_000);
    f.user("finish");
    let mut m = Mirror::open(&f.sess, &f.cfg).unwrap();
    m.prepare(&f.sess.read().unwrap(), &f.cfg).unwrap();
    assert_eq!(m.apply(&f.sess, &f.cfg).unwrap(), None, "правки нет");

    // До первой ревизии — честное «нечего применять».
    let first = Mirror::noop_outcome(&f.sess).unwrap();
    assert!(first.starts_with("context unchanged"), "{first}");

    let mut d = document(&m);
    let at = d
        .blocks
        .iter()
        .position(|b| b.messages.iter().any(|m| m["role"] == "tool"))
        .unwrap();
    d.blocks[at].messages[1]["content"] = json!("short");
    save(&m, &d);
    assert!(m
        .apply(&f.sess, &f.cfg)
        .unwrap()
        .unwrap()
        .starts_with("context applied:"));

    // После принятой ревизии повторный commit отвечает про принятую ревизию
    // и явно говорит, что повторять не нужно.
    let again = Mirror::noop_outcome(&f.sess).unwrap();
    assert!(
        again.starts_with("context already applied: revision "),
        "{again}"
    );
    assert!(again.contains("повторять commit не нужно"), "{again}");
}

/// Нечитаемый журнал — не «нечего применять», а сбой хода: тихое `unchanged`
/// скрыло бы от модели потерю состояния.
#[test]
fn unreadable_log_is_an_error_not_a_noop() {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "snens-context-broken-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let sess = Session::open(&dir, "broken").unwrap();
    let path = sess.path().to_path_buf();
    // Файл журнала подменён каталогом: открытие уже прошло, а чтение падает.
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let error = Mirror::noop_outcome(&sess).unwrap_err().to_string();
    assert!(error.contains("журнал сессии не читается"), "{error}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn forty_k_unelided_multiple_revisions_resume_and_append_only() {
    let f = Fixture::new();
    f.user("start");
    f.call("large", "run", json!({"args":["echo"]}));
    f.result("large", &"a".repeat(160_000));
    let original = fs::read(f.sess.path()).unwrap();
    let mut m = Mirror::open(&f.sess, &f.cfg).unwrap();
    let first = m.prepare(&f.sess.read().unwrap(), &f.cfg).unwrap();
    assert!(project::estimate_tokens(&first) >= 40_000);
    assert!(fs::read(m.path()).unwrap().len() > 160_000);
    let mut d = document(&m);
    assert!(!d
        .blocks
        .iter()
        .flat_map(|b| &b.messages)
        .any(|m| m["role"] == "system"));
    assert_eq!(d.blocks[1].messages.len(), 2);
    d.blocks[1].messages[0]["tool_calls"][0]["function"]["arguments"] = json!("{}");
    d.blocks[1].messages[1]["content"] = json!("short");
    save(&m, &d);
    assert!(m
        .apply(&f.sess, &f.cfg)
        .unwrap()
        .unwrap()
        .starts_with("context applied:"));
    assert!(fs::read(f.sess.path()).unwrap().starts_with(&original));
    f.user("later");
    f.assistant(&format!("LATE_ANALYSIS:{}", "l".repeat(40_000)));
    m.prepare(&f.sess.read().unwrap(), &f.cfg).unwrap();
    let stable = document(&m).blocks[0].id.clone();
    let mut d = document(&m);
    note(&mut d, "note two");
    // Снимаем второй крупный результат: без сжатия правка будет отклонена.
    let late = d
        .blocks
        .iter()
        .position(|b| {
            b.messages.iter().any(|m| {
                m["content"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("LATE_ANALYSIS"))
            })
        })
        .unwrap();
    d.blocks[late].messages[0]["content"] = json!("late summary");
    save(&m, &d);
    assert!(m
        .apply(&f.sess, &f.cfg)
        .unwrap()
        .unwrap()
        .starts_with("context applied:"));
    let expected = effective(&f.sess.read().unwrap(), &f.cfg).unwrap();
    let mut resumed = Mirror::open(&f.sess, &f.cfg).unwrap();
    assert_eq!(
        resumed.prepare(&f.sess.read().unwrap(), &f.cfg).unwrap(),
        expected
    );
    assert_eq!(document(&resumed).blocks[0].id, stable);
    assert_eq!(resumed.apply(&f.sess, &f.cfg).unwrap(), None);
    assert_eq!(
        f.sess
            .read()
            .unwrap()
            .iter()
            .filter(|r| matches!(r, Rec::ContextRevision { .. }))
            .count(),
        2
    );
}

#[test]
fn malformed_stale_and_invalid_arguments_are_rejected_without_log_changes() {
    let f = Fixture::new();
    f.user("one");
    f.call("c", "run", json!({"args":["true"]}));
    f.result("c", "ok");
    f.user("two");
    let mut m = Mirror::open(&f.sess, &f.cfg).unwrap();
    let before = fs::read(f.sess.path()).unwrap();
    fs::write(m.path(), "{").unwrap();
    assert!(m
        .apply(&f.sess, &f.cfg)
        .unwrap()
        .unwrap()
        .contains("malformed"));
    let mut d = document(&m);
    d.revision = "old".into();
    save(&m, &d);
    let outcome = m.apply(&f.sess, &f.cfg).unwrap().unwrap();
    assert!(outcome.contains("stale"), "{outcome}");
    assert!(outcome.contains("reread LIVE_CONTEXT.json"), "{outcome}");
    let mut d = document(&m);
    d.blocks[1].messages[0]["tool_calls"][0]["function"]["arguments"] = json!("{broken");
    save(&m, &d);
    assert!(m
        .apply(&f.sess, &f.cfg)
        .unwrap()
        .unwrap()
        .contains("valid JSON"));
    assert_eq!(fs::read(f.sess.path()).unwrap(), before);
}

#[test]
fn opaque_images_restored_and_unknown_references_rejected() {
    let image = json!({"type":"image_url", "image_url":{"url":"data:image/png;base64,SECRET", "detail":"high"}});
    let messages = vec![
        json!({"role":"system","content":"sys"}),
        json!({"role":"user","content":[{"type":"text","text":"look"},image]}),
    ];
    let view = View::new(&messages, "r".into()).unwrap();
    assert!(!serde_json::to_string(&view.document)
        .unwrap()
        .contains("SECRET"));
    // Запрос пользователя защищён, даже если он единственный: это и первый, и
    // последний — правка лица задачи не проходит.
    let mut d = view.document.clone();
    d.blocks[0].messages[0]["content"][0]["text"] = json!("edited");
    let error = view.restore(&d).unwrap_err().to_string();
    assert!(error.contains("first and last"), "{error}");
    // Ссылка на несуществующее вложение по-прежнему отвергается.
    let mut d = view.document.clone();
    d.blocks[0].messages[0]["content"][1]["$context_attachment"] = json!("unknown");
    assert!(view.restore(&d).is_err());
    // Нетронутый документ восстанавливается, вложение на месте.
    let restored = view.restore(&view.document.clone()).unwrap();
    assert_eq!(restored[1]["content"][1], image);
}

#[test]
fn late_suffix_kept_but_mirror_service_calls_removed() {
    let f = Fixture::new();
    f.user("base");
    bulk(&f, 30_000);
    f.user("finish");
    let mut m = Mirror::open(&f.sess, &f.cfg).unwrap();
    let mut d = document(&m);
    let at = d
        .blocks
        .iter()
        .position(|b| b.messages.iter().any(|m| m["role"] == "tool"))
        .unwrap();
    d.blocks[at].messages[1]["content"] = json!("short");
    save(&m, &d);
    f.call(
        "edit",
        "edit",
        json!({"args":[m.path().to_str().unwrap()], "stdin":"draft payload"}),
    );
    f.result("edit", "edited");
    f.call("ordinary", "run", json!({"args":["echo ok"]}));
    f.result("ordinary", "normal output");
    f.call("commit", "ctx", json!({"args":["commit"]}));
    f.result("commit", "commit requested");
    let outcome = m.apply(&f.sess, &f.cfg).unwrap().unwrap();
    assert!(outcome.starts_with("context applied:"), "{outcome}");
    let messages = effective(&f.sess.read().unwrap(), &f.cfg).unwrap();
    let text = serde_json::to_string(&messages).unwrap();
    assert!(!text.contains("draft payload"));
    assert!(!text.contains("commit requested"));
    assert!(text.contains("normal output"));
    assert_eq!(messages.iter().filter(|m| m["role"] == "system").count(), 1);
    assert_eq!(
        messages
            .iter()
            .filter(|m| m["tool_call_id"] == "ordinary")
            .count(),
        1
    );
}

#[test]
fn raw_prefix_digest_checked_on_resume() {
    let f = Fixture::new();
    f.user("base");
    bulk(&f, 30_000);
    f.user("finish");
    let mut m = Mirror::open(&f.sess, &f.cfg).unwrap();
    let mut d = document(&m);
    assert!(shrink_bulk(&mut d));
    save(&m, &d);
    m.apply(&f.sess, &f.cfg).unwrap();
    let mut raw = f.sess.read().unwrap();
    let tampered = raw
        .iter_mut()
        .find_map(|rec| match rec {
            Rec::User { text, .. } if text == "base" => Some(text),
            _ => None,
        })
        .expect("base user record");
    *tampered = "tampered".into();
    assert!(effective(&raw, &f.cfg).is_err());
}

#[test]
fn repaired_tail_can_complete_after_revision() {
    let f = Fixture::new();
    f.user("base");
    f.call("big", "run", json!({"args":["true"]}));
    f.result("big", &"b".repeat(40_000));
    f.user("finish");
    let mut m = Mirror::open(&f.sess, &f.cfg).unwrap();
    let mut d = document(&m);
    let at = d
        .blocks
        .iter()
        .position(|b| b.messages.iter().any(|m| m["tool_call_id"] == "big"))
        .unwrap();
    d.blocks[at].messages[1]["content"] = json!("short");
    save(&m, &d);
    m.apply(&f.sess, &f.cfg).unwrap();
    f.call("c", "run", json!({"args":["true"]}));
    m.prepare(&f.sess.read().unwrap(), &f.cfg).unwrap();
    f.result("c", "finished");
    let messages = m.prepare(&f.sess.read().unwrap(), &f.cfg).unwrap();
    assert_eq!(
        messages.iter().filter(|m| m["tool_call_id"] == "c").count(),
        1
    );
    assert!(messages.iter().any(|m| m["content"] == "finished"));
}

#[test]
fn clean_older_mirror_refreshes_but_stale_edited_draft_rejected_on_resume() {
    let f = Fixture::new();
    f.user("base");
    let m = Mirror::open(&f.sess, &f.cfg).unwrap();
    let clean = document(&m);
    f.user("new raw suffix");
    let mut resumed = Mirror::open(&f.sess, &f.cfg).unwrap();
    assert_ne!(document(&resumed).revision, clean.revision);
    assert_eq!(resumed.apply(&f.sess, &f.cfg).unwrap(), None);
    let mut draft = clean;
    note(&mut draft, "stale draft");
    save(&resumed, &draft);
    let mut resumed = Mirror::open(&f.sess, &f.cfg).unwrap();
    assert!(resumed
        .apply(&f.sess, &f.cfg)
        .unwrap()
        .unwrap()
        .contains("stale"));
}

#[test]
fn baseline_mirror_reads_scrubbed_status_and_mixed_calls_preserved() {
    let f = Fixture::new();
    f.user("base");
    bulk(&f, 30_000);
    f.user("finish");
    let m = Mirror::open(&f.sess, &f.cfg).unwrap();
    f.call("read", "read", json!({"args":[m.path().to_str().unwrap()]}));
    f.result("read", "MIRROR_PAYLOAD");
    f.call("status", "ctx", json!({"action":"status"}));
    f.result("status", "STATUS_RESULT");
    let mut m = Mirror::open(&f.sess, &f.cfg).unwrap();
    let mut draft = document(&m);
    let at = draft
        .blocks
        .iter()
        .position(|b| b.messages.iter().any(|m| m["role"] == "tool"))
        .unwrap();
    draft.blocks[at].messages[1]["content"] = json!("short");
    save(&m, &draft);
    f.sess
        .append(Rec::Assistant {
            ts: 1,
            text: Some("mixed assistant text".into()),
            tool_calls: vec![
                Call {
                    id: "commit".into(),
                    name: "ctx".into(),
                    args: json!({"action":"commit"}).to_string(),
                },
                Call {
                    id: "normal".into(),
                    name: "run".into(),
                    args: "{}".into(),
                },
            ],
            reasoning: None,
        })
        .unwrap();
    f.result("commit", "COMMIT_RESULT");
    f.result("normal", "ORDINARY_RESULT");
    assert!(m
        .apply(&f.sess, &f.cfg)
        .unwrap()
        .unwrap()
        .starts_with("context applied:"));
    let text = serde_json::to_string(&effective(&f.sess.read().unwrap(), &f.cfg).unwrap()).unwrap();
    assert!(!text.contains("MIRROR_PAYLOAD"));
    assert!(!text.contains("COMMIT_RESULT"));
    assert!(text.contains("STATUS_RESULT"));
    assert!(text.contains("ORDINARY_RESULT"));
    assert!(text.contains("mixed assistant text"));
    assert!(service_call("ctx", &json!({}), m.path()));
    assert!(!service_call("ctx", &json!({"action":"status"}), m.path()));
}

#[test]
fn draft_survives_separate_edit_and_commit_steps() {
    let f = Fixture::new();
    f.user("keep task");
    bulk(&f, 30_000);
    f.user("finish");
    let mut mirror = Mirror::open(&f.sess, &f.cfg).unwrap();
    let mut draft = document(&mirror);
    note(&mut draft, "continuation");
    let at = draft
        .blocks
        .iter()
        .position(|b| b.messages.iter().any(|m| m["role"] == "tool"))
        .unwrap();
    draft.blocks[at].messages[1]["content"] = json!("short");
    save(&mirror, &draft);
    f.call(
        "edit-step",
        "edit",
        json!({"args":[mirror.path().to_string_lossy()],"stdin":"payload"}),
    );
    f.result("edit-step", "written");
    mirror.prepare(&f.sess.read().unwrap(), &f.cfg).unwrap();
    assert_eq!(document(&mirror), draft);
    f.call("commit-step", "ctx", json!({"action":"commit"}));
    f.result("commit-step", "scheduled");
    assert!(mirror
        .apply(&f.sess, &f.cfg)
        .unwrap()
        .unwrap()
        .starts_with("context applied:"));
    let messages = effective(&f.sess.read().unwrap(), &f.cfg).unwrap();
    assert!(messages.iter().any(|m| m["content"] == "continuation"));
    assert!(!serde_json::to_string(&messages)
        .unwrap()
        .contains("edit-step"));
}

/// Assistant-текст из середины удаляется: CLM, контекст принадлежит модели.
#[test]
fn mid_dialog_assistant_text_is_editable() {
    let f = Fixture::new();
    f.user("objective");
    f.assistant(&format!("STALE_ANALYSIS:{}", "y".repeat(30_000)));
    f.assistant("next step planned");
    f.user("instruction");
    let mut m = Mirror::open(&f.sess, &f.cfg).unwrap();
    let mut d = document(&m);
    let at = d
        .blocks
        .iter()
        .position(|b| {
            b.messages.iter().any(|m| {
                m["content"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("STALE_ANALYSIS"))
            })
        })
        .unwrap();
    d.blocks[at].messages[0]["content"] = json!("summary: analysis done, decision kept");
    save(&m, &d);
    let outcome = f.apply(&mut m);
    assert!(outcome.starts_with("context applied:"), "{outcome}");
    let text = serde_json::to_string(&effective(&f.sess.read().unwrap(), &f.cfg).unwrap()).unwrap();
    assert!(text.contains("summary: analysis done"));
    assert!(!text.contains("STALE_ANALYSIS"));
    assert!(text.contains("next step planned"));
}
