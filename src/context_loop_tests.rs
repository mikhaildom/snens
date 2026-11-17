//! End-to-end context editing through the real agent loop and builtin ctx dispatch.
//! No provider, external tools, or tokenizer: sizes use the harness's own estimator.
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};

use serde_json::{json, Value};

use crate::{
    agent::run_agent,
    cfg::Config,
    context,
    llm::{Delta, Llm, Reply},
    log::{Call, Rec, Session},
    project,
    render::Ui,
    R,
};

const GOAL: &str = "Finish the task using exact datum NEEDLE-739.";
const NOTE: &str = "Continuation note: goal unchanged; NEEDLE-739 verified; next action: finish.";
// Один большой результат тула: именно он и есть переваренное, что уборка снимает.
// Запрос пользователя и текст ответа модели остаются неприкосновенными.
const BULK_TOOL: &str = "DISCARDED_TOOL_PAYLOAD:";
static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    dir: PathBuf,
    cfg: Config,
    sess: Option<Session>,
    session_id: String,
    original: Vec<u8>,
}

impl Fixture {
    fn new(label: &str, large: bool) -> R<Self> {
        let dir = std::env::temp_dir().join(format!(
            "snens-context-loop-{label}-{}-{}-{}",
            std::process::id(),
            crate::log::now(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(dir.join("tools"))?;
        let cfg = Config {
            sessions_dir: dir.to_string_lossy().into_owned(),
            tools_dir: dir.join("tools").to_string_lossy().into_owned(),
            mail_dir: dir.join("mail").to_string_lossy().into_owned(),
            skills_dir: dir.join("skills").to_string_lossy().into_owned(),
            ctx_window: 100_000,
            context_budget: 40_000,
            context_target: 28_000,
            // Большой результат тула должен дойти до проекции целиком: уборка
            // проверяется на настоящем объёме, а не на обрезке shell-хвоста.
            max_result_chars: 500_000,
            // Пять последних результатов уборка не трогает: единственный крупный
            // результат защищён, поэтому автоэлизия молчит, а чистит модель.
            keep_results: 5,
            // В маленьком сценарии сжимать нечего: проверяется видимость отказа,
            // а не механический фильтр сжатия (он — в unit-тестах).
            context_min_savings: if large { Some(2000) } else { Some(1) },
            max_turns: 8,
            ..Config::default()
        };
        assert_eq!(cfg.context_budget(), 40_000);
        assert_eq!(cfg.context_target(), 28_000);
        let session_id = format!(
            "context-loop-{}-{}",
            std::process::id(),
            NEXT_DIR.load(Ordering::Relaxed)
        );
        let sess = Session::open(&dir, &session_id)?;
        sess.append(Rec::System {
            ts: 0,
            text: "Small fixed system.".into(),
        })?;
        sess.append(Rec::User {
            ts: 0,
            text: GOAL.into(),
            source: "test".into(),
            parts: Vec::new(),
            ask: None,
        })?;
        if large {
            // 240k байт в результате тула: порог перейдён (>= 40k + бюджет).
            // Уборка тут — снять переваренный вывод; диалог не трогается, потому
            // что решения и ответы как раз в нём. max_result_chars не спасает:
            // это самый обычный крупный результат, а не усечённый shell-хвост.
            sess.append(Rec::Assistant {
                ts: 0,
                text: None,
                tool_calls: vec![Call {
                    id: "bulk".into(),
                    name: "run".into(),
                    args: json!({"command": "cat bulk"}).to_string(),
                }],
                reasoning: None,
            })?;
            sess.append(Rec::ToolResult {
                ts: 0,
                call_id: "bulk".into(),
                tool: "run".into(),
                argv: vec![json!({"command": "cat bulk"}).to_string()],
                exit: 0,
                bytes: 240_000,
                ms: 0,
                body: crate::log::Body::Inline {
                    text: format!("{BULK_TOOL}{}", "x".repeat(240_000)),
                },
                parts: Vec::new(),
            })?;
            let tokens = project::estimate_tokens(&context::effective(&sess.read()?, &cfg)?);
            assert!(tokens >= 40_000, "seed only {tokens} tokens");
            assert!(tokens < cfg.ctx_window - cfg.context_reserve);
        }
        let original = fs::read(sess.path())?;
        Ok(Self {
            dir,
            cfg,
            sess: Some(sess),
            session_id,
            original,
        })
    }

    fn log_path(&self) -> PathBuf {
        self.dir.join(format!("{}.jsonl", self.session_id))
    }

    fn mirror_path(&self) -> PathBuf {
        self.log_path()
            .with_extension("out")
            .join("LIVE_CONTEXT.json")
    }

    fn assert_append_only(&self) -> R<()> {
        assert!(
            fs::read(self.log_path())?.starts_with(&self.original),
            "original JSONL rewritten"
        );
        Ok(())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn commit(id: &str) -> Reply {
    Reply {
        text: String::new(),
        reasoning: String::new(),
        calls: vec![Call {
            id: id.into(),
            name: "ctx".into(),
            args: r#"{"action":"commit"}"#.into(),
        }],
        usage: Default::default(),
        finish_reason: "tool_calls".into(),
        upstream: Default::default(),
    }
}

fn final_reply() -> Reply {
    Reply {
        text: "Task complete: NEEDLE-739.".into(),
        calls: vec![],
        finish_reason: "stop".into(),
        ..commit("unused")
    }
}

fn compact_mirror(path: &Path) -> R<()> {
    // Fake models see provider msgs but edit the actual filesystem mirror during reply.
    // Уборка теперь хирургическая: удалить переваренные пары вызов→результат,
    // диалог (запрос пользователя, текст ответа) не трогать, добавить записку.
    let mut draft: Value = serde_json::from_slice(&fs::read(path)?)?;
    let blocks = draft["blocks"].as_array_mut().expect("mirror blocks");
    blocks.retain(|block| {
        !block["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["content"].as_str().is_some_and(|s| s.contains(BULK_TOOL)))
    });
    let goal = blocks
        .iter()
        .filter(|b| {
            b["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["content"] == GOAL)
        })
        .count();
    assert_eq!(goal, 1, "goal block missing");
    blocks.push(json!({"id":"new-continuation", "messages":[{"role":"user", "content":NOTE}]}));
    fs::write(path, serde_json::to_vec_pretty(&draft)?)?;
    Ok(())
}

fn assert_compacted(msgs: &[Value], cfg: &Config) {
    assert!(msgs.iter().any(|m| m["content"] == GOAL));
    assert!(msgs.iter().any(|m| m["content"] == NOTE));
    assert!(
        msgs.iter().any(|m| m["content"]
            .as_str()
            .is_some_and(|s| s.contains("context applied: revision"))),
        "acceptance not visible"
    );
    let wire = serde_json::to_string(msgs).unwrap();
    assert!(!wire.contains(BULK_TOOL));
    // Protocol system mentions ctx; assert absence of actual service calls/results,
    // not absence of the word in instructions.
    assert!(
        !msgs.iter().any(|m| m["role"] == "tool"),
        "ctx result leaked"
    );
    assert!(
        !msgs.iter().any(|m| m.get("tool_calls").is_some()),
        "ctx payload leaked"
    );
    assert!(project::estimate_tokens(msgs) < cfg.context_target());
}

enum Script {
    Compact,
    Recover,
    Resume(Vec<Value>),
}

struct EditingLlm {
    path: PathBuf,
    script: Script,
    seen: Mutex<Vec<Vec<Value>>>,
}

impl EditingLlm {
    fn new(path: PathBuf, script: Script) -> Self {
        Self {
            path,
            script,
            seen: Mutex::new(vec![]),
        }
    }
}

impl Llm for EditingLlm {
    async fn reply(
        &self,
        cfg: &Config,
        msgs: Vec<Value>,
        tools: Vec<Value>,
        _on_delta: &mut (dyn FnMut(Delta) + Send),
    ) -> R<Reply> {
        assert!(tools.iter().any(|t| t["function"]["name"] == "ctx"));
        assert!(self.path.is_file(), "mirror not prepared before request");
        let turn = {
            let mut seen = self.seen.lock().unwrap();
            let turn = seen.len();
            seen.push(msgs.clone());
            turn
        };
        match (&self.script, turn) {
            (Script::Compact, 0) => {
                assert!(project::estimate_tokens(&msgs) >= 40_000 + cfg.context_budget());
                assert!(msgs.iter().any(|m| {
                    m["content"]
                        .as_str()
                        .is_some_and(|s| s.contains("[Уборка рабочего контекста]"))
                }));
                // One unedited commit keeps pressure high for another turn, checking
                // reminder hysteresis rather than only an immediate successful cleanup.
                Ok(commit("commit-unedited"))
            }
            (Script::Compact, 1) => {
                assert!(project::estimate_tokens(&msgs) >= cfg.context_budget());
                compact_mirror(&self.path)?;
                Ok(commit("commit-applied"))
            }
            (Script::Recover, 0) => {
                fs::write(&self.path, b"{malformed mirror")?;
                Ok(commit("commit-malformed"))
            }
            (Script::Recover, 1) => {
                assert!(
                    msgs.iter().any(|m| m["content"]
                        .as_str()
                        .is_some_and(|s| s.contains("context rejected: malformed JSON"))),
                    "rejection not visible"
                );
                assert!(
                    !msgs.iter().any(|m| m["content"] == NOTE),
                    "invalid draft applied"
                );
                assert!(msgs.iter().any(|m| m["content"] == GOAL));
                // Rejection must have restored a parseable, current mirror.
                compact_mirror(&self.path)?;
                Ok(commit("commit-recovered"))
            }
            (Script::Compact | Script::Recover, 2) => {
                assert_compacted(&msgs, cfg);
                Ok(final_reply())
            }
            (Script::Resume(expected), 0) => {
                assert_compacted(&msgs, cfg);
                // Абзац про управление контекстом едет только на ходу уборки:
                // на resume его нет, и проекция совпадает с ожидаемой.
                let mut without_protocol = msgs;
                if without_protocol[1]["role"] == "system" {
                    without_protocol.remove(1);
                }
                assert_eq!(
                    &without_protocol, expected,
                    "resume changed effective state"
                );
                Ok(final_reply())
            }
            _ => panic!("unexpected provider turn {turn}"),
        }
    }
}

fn cleanup_count(records: &[Rec]) -> usize {
    records
        .iter()
        .filter(|r| matches!(r, Rec::User { source, .. } if source.starts_with("context-cleanup")))
        .count()
}

#[tokio::test]
async fn forty_k_budget_real_loop_compacts_and_resumes_without_replaying_payload() -> R<()> {
    let mut f = Fixture::new("large", true)?;
    let fake = EditingLlm::new(f.mirror_path(), Script::Compact);
    run_agent(
        f.sess.as_ref().expect("session open"),
        &f.cfg,
        &fake,
        &Ui::plain(),
    )
    .await?;
    assert_eq!(fake.seen.lock().unwrap().len(), 3);
    f.assert_append_only()?;
    let raw = f.sess.as_ref().expect("session open").read()?;
    assert_eq!(cleanup_count(&raw), 1, "reminder repeated above budget");
    assert_eq!(
        raw.iter()
            .filter(|r| matches!(r, Rec::ContextRevision { .. }))
            .count(),
        1
    );
    assert!(
        raw.iter()
            .any(|r| matches!(r, Rec::ToolResult { tool, exit: 0, .. } if tool == "ctx")),
        "builtin ctx was not dispatched"
    );
    // Журнал хранит настоящий результат целиком: уборка — правило проекции,
    // лог неизменяем, поэтому элизия обратима.
    assert!(raw.iter().any(
        |r| matches!(r, Rec::ToolResult { body: crate::log::Body::Inline { text }, .. }
            if text.starts_with(BULK_TOOL))
    ));
    assert!(matches!(raw.last(), Some(Rec::Done { status, .. }) if status == "ok"));

    // Reopen from disk, append a new task input and create a fresh run_agent/mirror.
    let before_resume = fs::read(f.sess.as_ref().expect("session open").path())?;
    f.sess.take();
    let resumed = Session::open(&f.dir, &f.session_id)?;
    resumed.append(Rec::User {
        ts: 0,
        source: "test-resume".into(),
        text: "Continue from the saved note.".into(),
        parts: Vec::new(),
        ask: None,
    })?;
    let expected = context::effective(&resumed.read()?, &f.cfg)?;
    let resume_fake = EditingLlm::new(f.mirror_path(), Script::Resume(expected));
    run_agent(&resumed, &f.cfg, &resume_fake, &Ui::plain()).await?;
    assert_eq!(resume_fake.seen.lock().unwrap().len(), 1);
    assert!(fs::read(resumed.path())?.starts_with(&before_resume));
    f.assert_append_only()?;
    let raw = resumed.read()?;
    assert!(cleanup_count(&raw) <= 1, "resume reissued cleanup reminder");
    assert_compacted(&context::effective(&raw, &f.cfg)?, &f.cfg);
    assert!(matches!(raw.last(), Some(Rec::Done { status, .. }) if status == "ok"));
    Ok(())
}

#[tokio::test]
async fn malformed_context_draft_is_visible_and_model_can_recover_in_real_loop() -> R<()> {
    let f = Fixture::new("rejected", false)?;
    let fake = EditingLlm::new(f.mirror_path(), Script::Recover);
    run_agent(
        f.sess.as_ref().expect("session open"),
        &f.cfg,
        &fake,
        &Ui::plain(),
    )
    .await?;
    assert_eq!(fake.seen.lock().unwrap().len(), 3);
    f.assert_append_only()?;
    let raw = f.sess.as_ref().expect("session open").read()?;
    assert_eq!(
        raw.iter()
            .filter(|r| matches!(r, Rec::ContextRevision { .. }))
            .count(),
        1,
        "malformed draft persisted a revision"
    );
    assert_eq!(raw.iter().filter(|r| matches!(r, Rec::ContextNotice { text, .. } if text.contains("context rejected:"))).count(), 1);
    assert_compacted(&context::effective(&raw, &f.cfg)?, &f.cfg);
    assert!(matches!(raw.last(), Some(Rec::Done { status, .. }) if status == "ok"));
    let _: Value = serde_json::from_slice(&fs::read(f.mirror_path())?)?;
    Ok(())
}
