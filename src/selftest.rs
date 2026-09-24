// Интеграционная проверка ядра: единственная, что падает, если цикл сломан.
// Живёт отдельно от цикла: это тест-сценарий на реальных процессах, не код исполнения.
use serde_json::Value;

use crate::{
    agent::run_agent,
    cfg::Config,
    dispatch,
    llm::{Fake, Reply, Usage},
    log,
    log::{Body, Call, Rec, Session},
    project, R,
};

/// Единственная проверка, которая падает, если цикл сломан: ход с tool_call →
/// результат → финальный ответ, плюс реестр (хелп-контракт, argv, stdin, PATH)
/// и корректная проекция.
pub async fn selftest() -> R<()> {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("snens-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    // Фальшивый тул: реестр, хелп-контракт, argv, stdin, PATH — всё настоящее.
    let tdir = dir.join("tools/probe");
    std::fs::create_dir_all(&tdir)?;
    let probe = tdir.join("probe");
    std::fs::write(
        &probe,
        "#!/bin/sh\n\
         if [ \"$1\" = \"--help\" ]; then\n\
         \x20 echo \"probe — тестовый тул: печатает argv и stdin.\"\n\
         \x20 echo \"probe [args...] (stdin)\"\n\
         \x20 exit 0\n\
         fi\n\
         echo \"args:$*\"\n\
         cat\n",
    )?;
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755))?;

    let cfg = Config {
        sessions_dir: dir.to_string_lossy().into_owned(),
        tools_dir: dir.join("tools").to_string_lossy().into_owned(),
        ..Config::default()
    };

    let (defs, reg, path) = dispatch::prepare(&cfg).await?;
    assert!(reg.iter().any(|t| t.name == "probe"), "реестр не нашёл тул");
    let pd = defs
        .iter()
        .find(|d| d["function"]["name"] == "probe")
        .expect("нет определения probe");
    let desc = pd["function"]["description"].as_str().unwrap_or_default();
    assert!(desc.starts_with("probe —"), "описание не из --help: {desc}");
    assert!(
        desc.contains("probe [args...]"),
        "в описание не попал usage"
    );
    // usage со словом «stdin» → требование stdin доезжает в схему, а не через exit=1.
    assert!(
        desc.contains("stdin обязателен"),
        "в описание не попал признак stdin: {desc}"
    );
    let req = pd["function"]["parameters"]["required"]
        .as_array()
        .expect("required не массив");
    assert!(
        req.iter().any(|v| v == "stdin"),
        "stdin не помечен обязательным"
    );
    assert!(
        std::fs::symlink_metadata(cfg.bin_path().join("probe")).is_ok(),
        "нет симлинка в PATH-каталоге"
    );
    // PATH детей начинается с каталога шимов (`sudo` → `sudo -A`), чтобы sudo
    // спрашивал пароль через askpass, а сразу за ним — каталог симлинков тулов.
    let shim_s = cfg.data_path().join("shim");
    let bin_s = cfg.bin_path();
    let comps: Vec<std::path::PathBuf> = std::env::split_paths(&path).collect();
    assert_eq!(
        comps.first().map(|p| p.as_path()),
        Some(shim_s.as_path()),
        "PATH детей не начинается с каталога шимов: {path}"
    );
    assert!(
        comps.iter().any(|p| p == &bin_s),
        "PATH детей не содержит каталог симлинков тулов: {path}"
    );

    let sess = Session::open(&dir, "t")?;
    sess.append(Rec::User {
        ts: log::now(),
        text: "проверь тулы".into(),
        source: "selftest".into(),
    })?;

    let fake = Fake::new(vec![
        Reply {
            reasoning: String::new(),
            upstream: Default::default(),
            text: String::new(),
            calls: vec![Call {
                id: "c1".into(),
                name: "probe".into(),
                args: r#"{"args":["a","b"],"stdin":"привет"}"#.into(),
            }],
            usage: Usage::default(),
            finish_reason: String::new(),
        },
        Reply {
            reasoning: String::new(),
            upstream: Default::default(),
            text: String::new(),
            calls: vec![Call {
                id: "c2".into(),
                name: "nosuch".into(),
                args: r#"{"args":[]}"#.into(),
            }],
            usage: Usage::default(),
            finish_reason: String::new(),
        },
        Reply {
            reasoning: String::new(),
            upstream: Default::default(),
            text: String::new(),
            calls: vec![Call {
                id: "c3".into(),
                name: "run".into(),
                args: r#"{"command":"probe via-path"}"#.into(),
            }],
            usage: Usage::default(),
            finish_reason: String::new(),
        },
        Reply {
            reasoning: String::new(),
            upstream: Default::default(),
            text: "готово".into(),
            calls: vec![],
            usage: Usage::default(),
            finish_reason: String::new(),
        },
    ]);

    run_agent(&sess, &cfg, &fake, &crate::render::Ui::plain()).await?;
    let recs = sess.read()?;
    let tool_text = |call: &str| -> Option<String> {
        recs.iter().find_map(|r| match r {
            Rec::ToolResult {
                tool,
                body: Body::Inline { text },
                ..
            } if tool == call => Some(text.clone()),
            _ => None,
        })
    };

    let probe_out = tool_text("probe").expect("нет результата probe");
    assert!(
        probe_out.contains("args:a b"),
        "argv не доехал: {probe_out}"
    );
    assert!(probe_out.contains("привет"), "stdin не доехал: {probe_out}");
    assert!(
        tool_text("nosuch")
            .unwrap_or_default()
            .contains("нет в реестре"),
        "неизвестный тул не дал понятной ошибки"
    );
    let via_path = tool_text("run").unwrap_or_default();
    assert!(
        via_path.contains("args:via-path"),
        "тул не виден из PATH внутри run: {via_path}"
    );
    assert!(
        recs.iter().rev().any(|r| matches!(
            r,
            Rec::Assistant { text: Some(t), tool_calls, .. } if tool_calls.is_empty() && t.contains("готово")
        )),
        "нет финального ответа"
    );

    let msgs = project::msgs(&recs, &cfg);
    assert!(
        msgs.iter().any(|m| m["role"] == "tool"),
        "проекция потеряла tool-сообщение"
    );
    assert_eq!(
        msgs[0]["role"], "system",
        "проекция должна начинаться с system"
    );
    // Промпт сессии записан в лог один раз и проекция берёт его оттуда, а не
    // собирает заново: иначе правка файлов инстанса по ходу сессии сдвинула бы
    // начало промпта и убила префикс-кеш.
    let systems: Vec<&str> = recs
        .iter()
        .filter_map(|r| match r {
            Rec::System { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(systems.len(), 1, "запись System должна быть ровно одна");
    assert_eq!(
        msgs[0]["content"].as_str().unwrap_or_default(),
        systems[0],
        "проекция собрала промпт заново вместо чтения из лога"
    );

    // повторная проекция обязана быть байт-в-байт той же (иначе кеш промахивается)
    let a = serde_json::to_string(&project::msgs(&recs, &cfg))?;
    let b = serde_json::to_string(&project::msgs(&sess.read()?, &cfg))?;
    assert_eq!(a, b, "проекция недетерминирована");

    // --- компакция: элизия старых результатов при переполнении окна ---
    let mut cfg2 = cfg.clone();
    cfg2.ctx_window = 400; // крошечное окно: мягкий порог пройден сразу
    cfg2.keep_results = 1;
    let sess2 = Session::open(&dir, "t2")?;
    sess2.append(Rec::User {
        ts: log::now(),
        text: "два больших вывода".into(),
        source: "selftest".into(),
    })?;
    let big = || Reply {
        reasoning: String::new(),
        upstream: Default::default(),
        text: String::new(),
        calls: vec![Call {
            id: "b".into(),
            name: "run".into(),
            args: r#"{"command":"seq 1 3000"}"#.into(),
        }],
        usage: Usage::default(),
        finish_reason: String::new(),
    };
    let fake2 = Fake::new(vec![
        big(),
        big(),
        Reply {
            reasoning: String::new(),
            upstream: Default::default(),
            text: "готово".into(),
            calls: vec![],
            usage: Usage::default(),
            finish_reason: String::new(),
        },
    ]);
    run_agent(&sess2, &cfg2, &fake2, &crate::render::Ui::plain()).await?;
    let recs2 = sess2.read()?;
    assert!(
        recs2.iter().any(|r| matches!(r, Rec::Ctx { .. })),
        "компакция не записала Ctx в журнал"
    );
    let msgs2 = project::msgs(&recs2, &cfg2);
    let tool_msgs: Vec<&Value> = msgs2.iter().filter(|m| m["role"] == "tool").collect();
    assert_eq!(
        tool_msgs.len(),
        2,
        "элизия не должна выбрасывать сообщения: assistant с tool_calls обязан иметь ответы"
    );
    let first = tool_msgs[0]["content"].as_str().unwrap_or_default();
    let last = tool_msgs[1]["content"].as_str().unwrap_or_default();
    assert!(
        first.contains("вырезан при компакции"),
        "старый результат не вырезан: {first}"
    );
    assert!(
        last.starts_with('1'),
        "последний результат должен остаться как есть"
    );
    let raw_bytes: usize = recs2
        .iter()
        .filter_map(|r| match r {
            Rec::ToolResult {
                body: Body::Inline { text },
                ..
            } => Some(text.len()),
            _ => None,
        })
        .sum();
    assert!(
        project::estimate_tokens(&msgs2) * 3 < raw_bytes as u64,
        "элизия не уменьшила проекцию: {} Б против {raw_bytes} Б в логе",
        project::estimate_tokens(&msgs2) * 3
    );
    assert_eq!(
        serde_json::to_string(&project::msgs(&recs2, &cfg2))?,
        serde_json::to_string(&project::msgs(&sess2.read()?, &cfg2))?,
        "проекция с элизией недетерминирована"
    );

    // --- долгая память: ядро знает только argv-контракт, провайдер — тул ---
    let mdir = dir.join("tools/memory");
    std::fs::create_dir_all(&mdir)?;
    let mbin = mdir.join("memory");
    let capture = dir.join("turn.json");
    std::fs::write(
        &mbin,
        format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
             \x20 --help) echo \"memory — тестовый провайдер памяти\"; echo \"memory recall|retain\"; exit 0;;\n\
             \x20 recall) echo \"факт: $2\"; exit 0;;\n\
             \x20 retain) cat > {}; echo \"op t1\"; exit 0;;\n\
             esac\n",
            capture.display()
        ),
    )?;
    std::fs::set_permissions(&mbin, std::fs::Permissions::from_mode(0o755))?;

    let mut cfg3 = cfg.clone();
    // Канал выключен по умолчанию: без программы ядро не зовёт ничего.
    let sess_off = Session::open(&dir, "t3-off")?;
    sess_off.append(Rec::User {
        ts: log::now(),
        text: "проверь память".into(),
        source: "selftest".into(),
    })?;
    let fake_off = Fake::new(vec![Reply {
        reasoning: String::new(),
        upstream: Default::default(),
        text: "готово".into(),
        calls: vec![],
        usage: Usage::default(),
        finish_reason: String::new(),
    }]);
    run_agent(&sess_off, &cfg3, &fake_off, &crate::render::Ui::plain()).await?;
    assert!(
        !sess_off
            .read()?
            .iter()
            .any(|r| matches!(r, Rec::Recall { .. })),
        "выключенный канал всё равно впрыснул справку"
    );

    cfg3.memory.cmd = "memory".into();
    let sess3 = Session::open(&dir, "t3")?;
    sess3.append(Rec::User {
        ts: log::now(),
        text: "проверь память".into(),
        source: "selftest".into(),
    })?;
    let fake3 = Fake::new(vec![Reply {
        reasoning: String::new(),
        upstream: Default::default(),
        text: "готово".into(),
        calls: vec![],
        usage: Usage::default(),
        finish_reason: String::new(),
    }]);
    run_agent(&sess3, &cfg3, &fake3, &crate::render::Ui::plain()).await?;
    let recs3 = sess3.read()?;
    let recall = recs3
        .iter()
        .find_map(|r| match r {
            Rec::Recall { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("справка памяти не записана в журнал");
    assert!(
        recall.contains("факт: проверь память"),
        "запрос не доехал до провайдера: {recall}"
    );
    assert!(
        project::msgs(&recs3, &cfg3)
            .iter()
            .any(|m| m["role"] == "system"
                && m["content"].as_str().unwrap_or_default().contains("факт:")),
        "справка не попала в проекцию"
    );
    let sent = std::fs::read_to_string(&capture).unwrap_or_default();
    assert!(
        sent.contains("проверь память") && sent.contains("готово"),
        "ход не доехал до retain: {sent}"
    );
    // Ответ записан ровно один раз: повторный прогон не дублирует справку.
    assert_eq!(
        recs3
            .iter()
            .filter(|r| matches!(r, Rec::Recall { .. }))
            .count(),
        1,
        "справка впрыснута больше одного раза"
    );

    std::fs::remove_dir_all(&dir).ok();
    println!(
        "selftest ok: {} тулов в реестре, {} записей, {} сообщений в проекции, ~{} токенов",
        reg.len(),
        recs.len(),
        msgs.len(),
        project::estimate_tokens(&msgs)
    );
    Ok(())
}
