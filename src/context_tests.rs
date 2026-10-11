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
        let cfg =
            toml::from_str::<Config>("ctx_window = 40000\nmax_result_chars = 200000\n").unwrap();
        Self { dir, sess, cfg }
    }
    fn user(&self, text: &str) {
        self.sess
            .append(Rec::User {
                ts: 1,
                text: text.into(),
                source: "human".into(),
                parts: Vec::new(),
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
    m.prepare(&f.sess.read().unwrap(), &f.cfg).unwrap();
    let stable = document(&m).blocks[0].id.clone();
    let mut d = document(&m);
    note(&mut d, "note two");
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
fn malformed_stale_orphan_and_invalid_arguments_are_rejected_without_log_changes() {
    let f = Fixture::new();
    f.user("one");
    f.call("c", "run", json!({"args":["true"]}));
    f.result("c", "ok");
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
    assert!(m.apply(&f.sess, &f.cfg).unwrap().unwrap().contains("stale"));
    let mut d = document(&m);
    d.blocks[1].messages.remove(0);
    save(&m, &d);
    assert!(m
        .apply(&f.sess, &f.cfg)
        .unwrap()
        .unwrap()
        .contains("orphan"));
    let mut d = document(&m);
    d.blocks[1].messages[0]["tool_calls"][0]["function"]["arguments"] = json!("{broken");
    save(&m, &d);
    assert!(m
        .apply(&f.sess, &f.cfg)
        .unwrap()
        .unwrap()
        .contains("valid JSON"));
    assert_eq!(fs::read(f.sess.path()).unwrap(), before);
    // Диалог защищён: замена tool-пары на user-заметку отвергается — иначе
    // уборка возвращала бы себя назад и забывала решения (амнезия).
    let mut d = document(&m);
    d.blocks[1].messages = vec![json!({"role":"user", "content":"summary"})];
    save(&m, &d);
    assert!(m
        .apply(&f.sess, &f.cfg)
        .unwrap()
        .unwrap()
        .contains("cannot"));
    // Новую заметку добавить можно: это рабочая записка, а не переписывание хода.
    let mut d = document(&m);
    note(&mut d, "summary");
    save(&m, &d);
    assert!(m
        .apply(&f.sess, &f.cfg)
        .unwrap()
        .unwrap()
        .starts_with("context applied:"));
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
    let mut d = view.document.clone();
    // Текст запроса пользователя править нельзя — защита от амнезии.
    d.blocks[0].messages[0]["content"][0]["text"] = json!("edited");
    assert!(view.restore(&d).is_err());
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
    let mut m = Mirror::open(&f.sess, &f.cfg).unwrap();
    let mut d = document(&m);
    note(&mut d, "summary");
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
    assert!(m
        .apply(&f.sess, &f.cfg)
        .unwrap()
        .unwrap()
        .starts_with("context applied:"));
    let messages = effective(&f.sess.read().unwrap(), &f.cfg).unwrap();
    let text = serde_json::to_string(&messages).unwrap();
    assert!(!text.contains("draft payload"));
    assert!(!text.contains("commit requested"));
    assert!(text.contains("normal output"));
    assert!(text.contains("summary"));
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
    let mut m = Mirror::open(&f.sess, &f.cfg).unwrap();
    let mut d = document(&m);
    note(&mut d, "note");
    save(&m, &d);
    m.apply(&f.sess, &f.cfg).unwrap();
    let mut raw = f.sess.read().unwrap();
    if let Rec::User { text, .. } = &mut raw[2] {
        *text = "tampered".into();
    }
    assert!(effective(&raw, &f.cfg).is_err());
}

#[test]
fn repaired_tail_can_complete_after_revision() {
    let f = Fixture::new();
    f.user("base");
    f.call("c", "run", json!({"args":["true"]}));
    let mut m = Mirror::open(&f.sess, &f.cfg).unwrap();
    let mut d = document(&m);
    note(&mut d, "note");
    save(&m, &d);
    m.apply(&f.sess, &f.cfg).unwrap();
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
    let m = Mirror::open(&f.sess, &f.cfg).unwrap();
    f.call("read", "read", json!({"args":[m.path().to_str().unwrap()]}));
    f.result("read", "MIRROR_PAYLOAD");
    f.call("status", "ctx", json!({"action":"status"}));
    f.result("status", "STATUS_RESULT");
    let mut m = Mirror::open(&f.sess, &f.cfg).unwrap();
    let mut draft = document(&m);
    note(&mut draft, "note");
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
    let mut mirror = Mirror::open(&f.sess, &f.cfg).unwrap();
    let mut draft = document(&mirror);
    note(&mut draft, "continuation");
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
