// Действия батчем. Один вызов `web act` — несколько действий подряд, потому что
// каждый round-trip к модели стоит дороже самого действия, а адреса (`e12`)
// живут до следующего снимка: батч внутри одного снимка — единственный способ
// не переспрашивать дерево между кликом и вводом.
//
// Клик — настоящие события ввода по центру рамки элемента (CDP Input.*), а не
// `element.click()` из JS: сайты проверяют `isTrusted`, и синтетический клик из
// страницы его не даёт.
use serde_json::{json, Value};

use crate::{state, Args};

pub fn run(args: &Args) -> Result<(), String> {
    let mut l = crate::live(args)?;
    run_with(&mut l, args)
}

/// То же, но на уже открытом соединении: нужно `web probe`, который держит
/// сессию между постановкой счётчиков и действиями.
pub fn run_with(l: &mut crate::Live, args: &Args) -> Result<(), String> {
    let raw = args
        .flag("json")
        .map(str::to_string)
        .or_else(|| args.positional.first().cloned())
        .ok_or("web act: нужен --json '<массив действий>' или JSON вторым аргументом")?;
    let actions: Value =
        serde_json::from_str(&raw).map_err(|e| format!("web act: не JSON: {e}"))?;
    let list = actions.as_array().cloned().ok_or(
        "web act: ожидается массив действий, напр. [{\"click\":\"e3\"},{\"press\":\"Enter\"}]",
    )?;
    if list.is_empty() {
        return Err("web act: пустой список действий".into());
    }
    activate(l)?;
    // Адреса перезагружаем после каждого действия, способного сменить документ:
    // `navigate`/`eval`/клик с переходом оставляют остальные адреса батча
    // указывающими на старые backendNodeId.
    let mut refs = state::load_refs(&l.tab).unwrap_or_default();
    let mut done: Vec<String> = Vec::new();
    for (i, a) in list.iter().enumerate() {
        match apply(&mut l.cdp, &refs, a) {
            Ok(d) => done.push(d),
            Err(e) => {
                // Уже выполненные действия печатаем: иначе вызывающий не знает,
                // случился ли клик, и повторяет весь батч — с двойными кликами.
                for d in &done {
                    println!("✓ {d}");
                }
                return Err(format!(
                    "действие #{} ({a}) не выполнено: {e}; до него выполнено {} — не повторяй весь батч",
                    i + 1,
                    done.len()
                ));
            }
        }
        if invalidates_refs(a) {
            refs = state::load_refs(&l.tab).unwrap_or_default();
        }
    }
    for d in &done {
        println!("✓ {d}");
    }
    if !args.on("no-snap") {
        delta(l)?;
    }
    Ok(())
}

/// Ввод CDP доставляется активной вкладке браузера, а не той, чью сессию мы
/// держим. Если целевую не активировать, клики молча уходят в соседнюю
/// (проверено: в headless они достаются первой вкладке, а «видимая» их не
/// получает вовсе). Поэтому активируем дважды — на уровне браузера и на уровне
/// вкладки — и честно предупреждаем, если вкладка сообщает `hidden`: значит
/// ввод может уйти не туда.
fn activate(l: &mut crate::Live) -> Result<(), String> {
    let ws = l.ep.browser_ws()?;
    let mut b = crate::cdp::Cdp::connect(&ws, 15.0)?;
    // Ошибку активации не глотаем: иначе ввод уходит в другую вкладку, а батч
    // отчитывается успехом.
    if let Err(e) = b.call("Target.activateTarget", json!({"targetId": l.tab})) {
        eprintln!("внимание: не удалось активировать вкладку: {e}");
    }
    if let Err(e) = l.cdp.call("Page.bringToFront", Value::Null) {
        eprintln!("внимание: вкладка не выведена вперёд: {e}");
    }
    if let Ok(v) = l.cdp.eval("document.visibilityState") {
        if v.as_str() == Some("hidden") {
            eprintln!(
                "внимание: вкладка {} сообщает hidden — ввод может уйти в другую вкладку. \
                 Держи одну вкладку на сессию или работай в headed-браузере",
                // По символам, а не байтам: срез по байту паниковал бы на
                // многобайтовом id — ровно на пути предупреждения.
                l.tab.chars().take(8).collect::<String>()
            );
        }
    }
    Ok(())
}

/// Меняет ли действие документ так, что адреса снимка могли устареть. Клик и
/// нажатие включаем намеренно: они часто ведут к навигации/перерисовке, а
/// лишняя перезагрузка адресов стоит одного чтения файла.
fn invalidates_refs(a: &Value) -> bool {
    let obj = match a {
        Value::Object(o) => o,
        _ => return false,
    };
    obj.contains_key("navigate")
        || obj.contains_key("eval")
        || obj.contains_key("click")
        || obj.contains_key("press")
        || obj.contains_key("set")
        || obj.contains_key("clear")
}

fn node_of(refs: &[crate::snap::Ref], addr: &str) -> Result<i64, String> {
    refs.iter()
        .find(|r| r.addr == addr)
        .map(|r| r.node)
        .ok_or_else(|| {
            format!("адреса {addr} нет в снимке (снимок устарел?): сделай `web snap` заново")
        })
}

fn apply(
    cdp: &mut crate::cdp::Cdp,
    refs: &[crate::snap::Ref],
    a: &Value,
) -> Result<String, String> {
    let obj = a.as_object().ok_or("act: действие не объект")?;
    let (name, arg) = obj.iter().next().ok_or("act: пустое действие")?;
    match name.as_str() {
        "click" => {
            let (x, y) = point(cdp, refs, arg)?;
            click_at(cdp, x, y)?;
            Ok(format!("клик {arg} ({x:.0},{y:.0})"))
        }
        "hover" => {
            let (x, y) = point(cdp, refs, arg)?;
            cdp.call(
                "Input.dispatchMouseEvent",
                json!({"type": "mouseMoved", "x": x, "y": y}),
            )?;
            Ok(format!("наведение {arg}"))
        }
        "type" => {
            let (addr, text) = ref_and_text(arg)?;
            let (x, y) = point(cdp, refs, &Value::String(addr.clone()))?;
            click_at(cdp, x, y)?;
            cdp.call("Input.insertText", json!({"text": text}))?;
            Ok(format!("ввод в {addr}: {} символов", text.chars().count()))
        }
        "clear" => {
            let addr = arg.as_str().ok_or("act clear: нужен адрес")?;
            let node = node_of(refs, addr)?;
            focus(cdp, node)?;
            // Выделить всё и затереть: у полей бывает маска/валидация на вставку.
            press(cdp, "Control+a")?;
            press(cdp, "Delete")?;
            Ok(format!("очищено {addr}"))
        }
        "press" => {
            let combo = arg.as_str().ok_or("act press: нужна строка клавиши")?;
            press(cdp, combo)?;
            Ok(format!("клавиша {combo}"))
        }
        "scroll" => {
            // Кривое значение — ошибка, а не тихие 600 px: иначе вызов выглядит
            // успешным, хотя прокрутил не туда (или не туда, куда просили).
            let dy = match arg {
                Value::Number(n) => n
                    .as_f64()
                    .ok_or_else(|| format!("act scroll: не число: {arg}"))?,
                Value::String(s) => match s.as_str() {
                    "up" => -600.0,
                    "down" => 600.0,
                    other => other
                        .parse()
                        .map_err(|_| format!("act scroll: ждёт up|down|число, получено {other}"))?,
                },
                Value::Object(o) => o
                    .get("dy")
                    .and_then(Value::as_f64)
                    .ok_or_else(|| format!("act scroll: нет числового dy: {arg}"))?,
                _ => return Err(format!("act scroll: непонятное значение {arg}")),
            };
            cdp.call(
                "Input.dispatchMouseEvent",
                json!({"type": "mouseWheel", "x": 10, "y": 10, "deltaX": 0, "deltaY": dy}),
            )?;
            Ok(format!("скролл {dy:.0}"))
        }
        "navigate" => {
            let url = arg.as_str().ok_or("act navigate: нужен URL")?;
            let r = cdp.call("Page.navigate", json!({"url": url, "transitionType": "typed"}))?;
            if let Some(err) = r.get("errorText").and_then(Value::as_str) {
                if !err.is_empty() {
                    return Err(format!("не открылось: {url}: {err}"));
                }
            }
            cdp.wait_ready(std::time::Duration::from_secs(15))?;
            Ok(format!("переход {url}"))
        }
        "wait" => {
            let ms = arg
                .as_u64()
                .ok_or_else(|| format!("act wait: ждёт целое число миллисекунд, получено {arg}"))?
                .min(30_000);
            std::thread::sleep(std::time::Duration::from_millis(ms));
            Ok(format!("пауза {ms} мс"))
        }
        "upload" => {
            let o = arg.as_object().ok_or("act upload: нужен {ref, file}")?;
            let addr = o.get("ref").and_then(Value::as_str).ok_or("upload: нет ref")?;
            let file = o.get("file").and_then(Value::as_str).ok_or("upload: нет file")?;
            let node = node_of(refs, addr)?;
            cdp.call("DOM.enable", Value::Null)?;
            cdp.call(
                "DOM.setFileInputFiles",
                json!({"files": [file], "backendNodeId": node}),
            )?;
            Ok(format!("файл в {addr}: {file}"))
        }
        "eval" => {
            let expr = arg.as_str().ok_or("act eval: нужна строка JS")?;
            let v = cdp.eval(expr)?;
            Ok(format!("eval → {}", crate::trunc(&v.to_string(), 200)))
        }
        other => Err(format!(
            "act: неизвестное действие {other} (click, type, clear, press, scroll, hover, navigate, wait, upload, eval)"
        )),
    }
}

fn ref_and_text(arg: &Value) -> Result<(String, String), String> {
    let o = arg.as_object().ok_or("act type: нужен {ref, text}")?;
    let addr = o
        .get("ref")
        .and_then(Value::as_str)
        .ok_or("type: нет ref")?;
    let text = o
        .get("text")
        .and_then(Value::as_str)
        .ok_or("type: нет text")?;
    Ok((addr.to_string(), text.to_string()))
}

/// Центр рамки элемента: сначала прокрутить в видимую область, потом
/// DOM.getBoxModel — оттуда берём content quad, а не «на глаз».
fn point(
    cdp: &mut crate::cdp::Cdp,
    refs: &[crate::snap::Ref],
    arg: &Value,
) -> Result<(f64, f64), String> {
    match arg {
        Value::String(addr) => {
            let node = node_of(refs, addr)?;
            cdp.call("DOM.enable", Value::Null)?;
            let _ = cdp.call("DOM.scrollIntoViewIfNeeded", json!({"backendNodeId": node}));
            let m = cdp.call("DOM.getBoxModel", json!({"backendNodeId": node}))?;
            let quad = m
                .get("model")
                .and_then(|x| x.get("content"))
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| {
                    format!("{addr}: у элемента нет рамки (скрыт или нулевой размер)")
                })?;
            let n: Vec<f64> = quad.iter().filter_map(Value::as_f64).collect();
            if n.len() < 8 {
                return Err(format!("{addr}: неполная рамка элемента"));
            }
            let x = (n[0] + n[2] + n[4] + n[6]) / 4.0;
            let y = (n[1] + n[3] + n[5] + n[7]) / 4.0;
            Ok((x, y))
        }
        Value::Array(a) if a.len() >= 2 => {
            let x = a[0].as_f64().ok_or("act: x не число")?;
            let y = a[1].as_f64().ok_or("act: y не число")?;
            Ok((x, y))
        }
        Value::Object(o) => {
            let x = o.get("x").and_then(Value::as_f64).ok_or("act: нет x")?;
            let y = o.get("y").and_then(Value::as_f64).ok_or("act: нет y")?;
            Ok((x, y))
        }
        _ => Err("act: ожидается адрес e12 или [x,y]".into()),
    }
}

fn click_at(cdp: &mut crate::cdp::Cdp, x: f64, y: f64) -> Result<(), String> {
    cdp.call(
        "Input.dispatchMouseEvent",
        json!({"type": "mouseMoved", "x": x, "y": y}),
    )?;
    cdp.call(
        "Input.dispatchMouseEvent",
        // buttons=1 (левая кнопка нажата): из битовой маски Chrome выводит
        // event.buttons, и canvas/drag-обработчики без неё видят 0.
        json!({"type": "mousePressed", "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1}),
    )?;
    cdp.call(
        "Input.dispatchMouseEvent",
        json!({"type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": 1}),
    )?;
    Ok(())
}

fn focus(cdp: &mut crate::cdp::Cdp, node: i64) -> Result<(), String> {
    cdp.call("DOM.enable", Value::Null)?;
    let r = cdp.call("DOM.resolveNode", json!({"backendNodeId": node}))?;
    let oid = r
        .get("object")
        .and_then(|o| o.get("objectId"))
        .and_then(Value::as_str)
        .ok_or("DOM.resolveNode: нет objectId")?;
    cdp.call(
        "Runtime.callFunctionOn",
        json!({
            "objectId": oid,
            "functionDeclaration": "function(){ this.focus && this.focus(); this.select && this.select(); }",
        }),
    )?;
    Ok(())
}

/// Клавиши: `windowsVirtualKeyCode` обязателен — без него `keyCode` в событиях
/// не тот, и сайты, которые на него смотрят, видят мусор.
fn key_for(name: &str) -> Option<(&'static str, u32, Option<char>)> {
    let t = match name {
        "Enter" | "Return" => ("Enter", 13, Some('\r')),
        "Tab" => ("Tab", 9, None),
        "Escape" | "Esc" => ("Escape", 27, None),
        "Backspace" => ("Backspace", 8, None),
        "Delete" => ("Delete", 46, None),
        "ArrowUp" => ("ArrowUp", 38, None),
        "ArrowDown" => ("ArrowDown", 40, None),
        "ArrowLeft" => ("ArrowLeft", 37, None),
        "ArrowRight" => ("ArrowRight", 39, None),
        "Home" => ("Home", 36, None),
        "End" => ("End", 35, None),
        "PageUp" => ("PageUp", 33, None),
        "PageDown" => ("PageDown", 34, None),
        "Space" | " " => (" ", 32, Some(' ')),
        _ => return None,
    };
    Some(t)
}

fn press(cdp: &mut crate::cdp::Cdp, combo: &str) -> Result<(), String> {
    let mut mods = 0u32;
    let mut key = combo;
    for part in combo.split('+') {
        match part.to_ascii_lowercase().as_str() {
            "control" | "ctrl" => mods |= 2,
            "alt" => mods |= 1,
            "shift" => mods |= 8,
            "meta" | "cmd" | "super" => mods |= 4,
            _ => key = part,
        }
    }
    // Одиночная буква/цифра в комбинации (Control+a): код берём из ASCII вверх
    // регистром — Chrome ждёт windowsVirtualKeyCode заглавной буквы.
    let (name, code, ch) = match key_for(key) {
        Some((n, c, ch)) => (n.to_string(), c, ch),
        None => {
            let mut cs = key.chars();
            match (cs.next(), cs.next()) {
                (Some(c), None) if c.is_ascii_alphanumeric() => {
                    let up = c.to_ascii_uppercase();
                    // Без Box::leak: имя клавиши живёт в этой функции, а утечка
                    // росла с каждым нажатием в батче.
                    (String::from(up), up as u32, Some(c))
                }
                _ => return Err(format!("press: неизвестная клавиша {key}")),
            }
        }
    };
    let base = json!({
        "key": name,
        "code": name,
        "windowsVirtualKeyCode": code,
        "nativeVirtualKeyCode": code,
        "modifiers": mods,
    });
    let mut down = base.clone();
    // keyDown, а не rawKeyDown: rawKeyDown определён без текста, и вставка символа
    // не происходит — `press("a")` не набирал букву в поле.
    down["type"] = json!("keyDown");
    if let Some(c) = ch {
        down["text"] = json!(c.to_string());
        down["unmodifiedText"] = json!(c.to_string());
    }
    cdp.call("Input.dispatchKeyEvent", down)?;
    let mut up = base;
    up["type"] = json!("keyUp");
    cdp.call("Input.dispatchKeyEvent", up)?;
    Ok(())
}

/// Ждём, пока страница перестанет быть «прежней»: клик мог запустить навигацию,
/// а `readyState=complete` у уходящего документа врёт — он готов ещё до коммита
/// новой навигации. Признак смены — другой URL либо готовность нового документа
/// при том же URL (SPA-перерисовка).
fn settle(l: &mut crate::Live) {
    let old = state::load_url(&l.tab);
    let until = std::time::Instant::now() + std::time::Duration::from_millis(4000);
    loop {
        let url = l
            .cdp
            .eval("location.href")
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        // `old` — None значит «снимка не было»: тогда ждать нечего.
        if old.as_deref() != Some(url.as_str()) && !url.is_empty() && old.is_some() {
            l.cdp
                .wait_ready(std::time::Duration::from_millis(3000))
                .ok();
            return;
        }
        if std::time::Instant::now() >= until {
            return; // переход не случился: страница та же, дельту считаем по ней
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
}

/// Дельта: после действий печатаем только изменившиеся строки дерева. Полный
/// снимок заново — это те же 10–16 КБ на каждый шаг, а меняется обычно 2–3 узла.
fn delta(l: &mut crate::Live) -> Result<(), String> {
    // Действие могло запустить навигацию — см. settle().
    settle(l);
    let before = state::load_page(&l.tab).unwrap_or_default();
    let prev_url = state::load_url(&l.tab);
    let s = crate::snap::take_stable(&mut l.cdp, 30)?;
    let path = state::save_page(&l.tab, &s.text)?;
    state::save_url(&l.tab, &s.url);
    let refs_path = state::save_refs(&l.tab, &s.refs)?;
    if prev_url.as_deref() != Some(s.url.as_str()) && !before.is_empty() && prev_url.is_some() {
        // Ушли на другую страницу: «дельта» между разными документами — шум.
        // Печатаем компактную сводку новой страницы, ссылки уже перезаписаны.
        println!(
            "страница сменилась: {} — {}",
            crate::trunc(&s.url, 110),
            crate::trunc(&s.title, 70)
        );
        println!(
            "дерево: {} узлов, интерактивных {} → {}",
            s.total,
            s.refs.len(),
            path.display()
        );
        for line in s.text.lines().take(15) {
            println!("{line}");
        }
        if s.text.lines().count() > 15 {
            println!(
                "… ещё {} строк: {}",
                s.text.lines().count() - 15,
                path.display()
            );
        }
        return Ok(());
    }
    let old: std::collections::HashSet<&str> = before.lines().collect();
    let new: std::collections::HashSet<&str> = s.text.lines().collect();
    let added: Vec<&&str> = new.difference(&old).collect();
    let gone = old.difference(&new).count();
    println!(
        "после действий: {} добавлено, {} исчезло; адреса {}-{} → {}",
        added.len(),
        gone,
        s.refs.first().map(|r| r.addr.clone()).unwrap_or_default(),
        s.refs.last().map(|r| r.addr.clone()).unwrap_or_default(),
        refs_path.display()
    );
    for line in added.iter().take(15) {
        println!("+ {line}");
    }
    if added.len() > 15 {
        println!("… ещё {} строк: {}", added.len() - 15, path.display());
    }
    Ok(())
}
