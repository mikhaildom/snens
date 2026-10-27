//! Проверка артефакта: статика (lint) и браузер в той же песочнице, что у клиента.
//!
//! Зачем браузер обязателен: `lint` видит чужие теги и приватную сеть, но не
//! видит, что компонент падает при регистрации, что `getElementById` вернул
//! `null`, что скрипт бросил исключение до первого кадра. Без этой части модель
//! правит вслепую.
//!
//! Три правила, из-за которых проверка честная:
//!
//! 1. документ собирается как у клиента — `iframe sandbox="allow-scripts
//!    allow-forms"` (origin `null`), CSP первой строкой, библиотека до тела;
//! 2. ошибки ловятся ВНУТРИ песочницы шимом и приходят `postMessage`-ом:
//!    родитель, по правилам sandbox, консоль iframe не видит;
//! 3. снимок кладётся в канал вложений — модель смотрит глазами (`view`), а не
//!    догадывается.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::fence::{self, Artifact};
use crate::library;

/// Потолок размера тела: артефакт уезжает в контекст промпта, и раздутый фенс
/// лечится только компакцией.
pub const MAX_BYTES: usize = 128 * 1024;

/// Что не так: ошибка (выход 1) или предупреждение (выход 0).
#[derive(Debug, Clone, PartialEq)]
pub enum Level {
    Error,
    Warn,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub level: Level,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
    /// Что прислала песочница: ошибки консоли, исключения, размеры.
    pub console: Vec<String>,
    pub rendered: Option<Value>,
    pub screenshot: Option<PathBuf>,
}

impl Report {
    pub fn error(&mut self, text: impl Into<String>) {
        self.findings.push(Finding {
            level: Level::Error,
            text: text.into(),
        });
    }

    pub fn warn(&mut self, text: impl Into<String>) {
        self.findings.push(Finding {
            level: Level::Warn,
            text: text.into(),
        });
    }

    pub fn errors(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.level == Level::Error)
            .count()
    }
}

/// Теги, которые клиент рендерит сам или которые есть в HTML всегда. Всё
/// остальное обязано быть либо в библиотеке компонентов, либо это опечатка:
/// незарегистрированный тег молча отрисуется пустотой, и это самая частая
/// поломка артефакта.
const STANDARD: &[&str] = &[
    "a",
    "abbr",
    "address",
    "area",
    "article",
    "aside",
    "audio",
    "b",
    "base",
    "bdi",
    "bdo",
    "blockquote",
    "body",
    "br",
    "button",
    "canvas",
    "caption",
    "cite",
    "code",
    "col",
    "colgroup",
    "data",
    "datalist",
    "dd",
    "del",
    "details",
    "dfn",
    "dialog",
    "div",
    "dl",
    "dt",
    "em",
    "embed",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hgroup",
    "hr",
    "html",
    "i",
    "iframe",
    "img",
    "input",
    "ins",
    "kbd",
    "label",
    "legend",
    "li",
    "link",
    "main",
    "map",
    "mark",
    "menu",
    "meta",
    "meter",
    "nav",
    "noscript",
    "object",
    "ol",
    "optgroup",
    "option",
    "output",
    "p",
    "picture",
    "pre",
    "progress",
    "q",
    "rp",
    "rt",
    "ruby",
    "s",
    "samp",
    "script",
    "search",
    "section",
    "select",
    "slot",
    "small",
    "source",
    "span",
    "strong",
    "style",
    "sub",
    "summary",
    "sup",
    "svg",
    "table",
    "tbody",
    "td",
    "template",
    "textarea",
    "tfoot",
    "th",
    "thead",
    "time",
    "title",
    "tr",
    "track",
    "u",
    "ul",
    "var",
    "video",
    "wbr",
];

fn is_standard(tag: &str) -> bool {
    STANDARD.contains(&tag)
}

/// Теги, использованные в разметке. Грубый поиск по `<имя`, а не HTML-парсер:
/// задача — найти опечатку в имени компонента, а не построить дерево.
fn used_tags(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut end = start;
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'-' || bytes[end] == b':')
        {
            end += 1;
        }
        if end > start {
            let tag = body[start..end].to_ascii_lowercase();
            if !out.contains(&tag) {
                out.push(tag);
            }
        }
        i = end.max(i + 1);
    }
    out
}

/// Статическая проверка: всё, что видно без браузера.
pub fn lint(art: &Artifact, lib_tags: &[String]) -> Report {
    let mut r = Report::default();
    if art.open {
        r.error("фенс не закрыт: модель ещё печатает или забыла закрывающие бэктики");
    }
    for (key, _) in &art.head.raw {
        if !["id", "src", "net", "height", "lib"].contains(&key.as_str()) {
            r.error(format!(
                "неизвестный ключ заголовка `{key}`: опечатка молча ничего не делает"
            ));
        }
    }
    for host in &art.head.net {
        if fence::is_private_host(host) {
            r.error(format!(
                "net={host}: приватная сеть запрещена всегда — артефакт исполняется в браузере человека"
            ));
        }
    }
    if art.head.src.is_empty() && art.body.trim().is_empty() {
        r.warn("тело пустое и src не задан: артефакт нечего показывать");
    }
    if art.body.len() > MAX_BYTES {
        r.error(format!(
            "тело {} КБ больше потолка {} КБ: фенс едет в контекст промпта",
            art.body.len() / 1024,
            MAX_BYTES / 1024
        ));
    }
    if !art.head.src.is_empty() && !art.body.trim().is_empty() {
        r.error("заданы и src, и тело: клиент возьмёт файл, тело молча потеряется");
    }
    for tag in used_tags(&art.body) {
        if is_standard(&tag) || lib_tags.iter().any(|t| t == &tag) {
            continue;
        }
        r.error(format!(
            "тег <{tag}> не зарегистрирован: его нет ни в библиотеке genui, ни в HTML — \
             отрисуется пустотой"
        ));
    }
    // Внешние скрипты: только с домена, объявленного в net=. Иначе `<script src>`
    // грузился бы мимо CSP и молча падал.
    for src in attr_values(&art.body, "src") {
        let host = host_of(&src);
        let Some(host) = host else { continue };
        let declared = art.head.net.iter().any(|n| n.eq_ignore_ascii_case(&host));
        if !declared {
            r.error(format!(
                "<script src={src}>: домен {host} не объявлен в net= — CSP его заблокирует"
            ));
        }
    }
    r
}

/// Значения атрибута в тела фенса (только для `src`).
fn attr_values(body: &str, attr: &str) -> Vec<String> {
    let mut out = Vec::new();
    let needle = format!("{attr}=");
    let mut rest = body;
    while let Some(pos) = rest.find(&needle) {
        let after = &rest[pos + needle.len()..];
        let (value, tail) = if let Some(stripped) = after.strip_prefix('"') {
            match stripped.find('"') {
                Some(end) => (stripped[..end].to_string(), &stripped[end + 1..]),
                None => (stripped.to_string(), ""),
            }
        } else if let Some(stripped) = after.strip_prefix('\'') {
            match stripped.find('\'') {
                Some(end) => (stripped[..end].to_string(), &stripped[end + 1..]),
                None => (stripped.to_string(), ""),
            }
        } else {
            let end = after
                .find(|c: char| c.is_whitespace() || c == '>')
                .unwrap_or(after.len());
            (after[..end].to_string(), &after[end..])
        };
        if !value.is_empty() {
            out.push(value);
        }
        rest = tail;
    }
    out
}

fn host_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "https" && scheme != "http" {
        return None;
    }
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host);
    let host = host.split(':').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

// ---------------------------------------------------------------- документ

/// CSP — копия `artifactCsp` из клиента (`snens-web/src/lib/artifact.ts`).
fn csp(art: &Artifact) -> String {
    let nets: Vec<&String> = art
        .head
        .net
        .iter()
        .filter(|h| !fence::is_private_host(h))
        .collect();
    let script = if nets.is_empty() {
        "'unsafe-inline'".to_string()
    } else {
        format!(
            "'unsafe-inline' {}",
            nets.iter()
                .map(|h| format!("https://{h}"))
                .collect::<Vec<_>>()
                .join(" ")
        )
    };
    let connect = if nets.is_empty() {
        "'none'".to_string()
    } else {
        nets.iter()
            .map(|h| format!("https://{h}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    [
        "default-src 'none'".to_string(),
        "style-src 'unsafe-inline'".to_string(),
        "img-src https: data: blob:".to_string(),
        "font-src data:".to_string(),
        "media-src data: blob:".to_string(),
        format!("script-src {script}"),
        format!("connect-src {connect}"),
        "form-action 'none'".to_string(),
        "base-uri 'none'".to_string(),
    ]
    .join("; ")
}

/// Шим песочницы: то же, что в клиенте, плюс доклад о готовности. Единственный
/// способ узнать, что документ сломался, — он сам об этом скажет.
const SHIM: &str = r#"<script>
(function(){
  var post=function(m){m.snens=1;try{parent.postMessage(m,"*")}catch(e){}};
  window.snens={data:null,answered:[],ask:function(o){post(Object.assign({type:"ask"},o||{}))},
    answer:function(o){post(Object.assign({type:"answer"},o||{}))},send:function(){},note:function(){}};
  var said={};
  function report(m,line){
    var key=String(m).slice(0,200)+"@"+line;
    if(said[key])return;said[key]=1;
    post({type:"error",message:String(m),line:line||0});
  }
  window.addEventListener("error",function(e){
    report(e.message||(e.target&&e.target.src)||"ошибка загрузки",e.lineno||0);
  },true);
  window.addEventListener("unhandledrejection",function(e){report("unhandled rejection: "+e.reason,0)});
  var ce=console.error;
  console.error=function(){try{report(Array.prototype.map.call(arguments,String).join(" "),0)}catch(x){}
    return ce.apply(console,arguments)};
  // Текст с учётом shadow DOM: компонент библиотеки обычно живёт в shadowRoot,
  // и innerText его не видит — «документ пустой» был бы ложной тревогой.
  function textOf(node){
    if(!node)return 0;
    var n=(node.innerText||node.textContent||"").length;
    var all=node.querySelectorAll?node.querySelectorAll("*"):[];
    for(var i=0;i<all.length;i++){if(all[i].shadowRoot)n+=textOf(all[i].shadowRoot);}
    return n;
  }
  function done(){
    var imgs=[].slice.call(document.images).filter(function(i){return !i.complete||!i.naturalWidth}).length;
    post({type:"ready", info:{
      text:textOf(document.body),
      elements:document.querySelectorAll("*").length,
      // Высота содержимого, а не окна: у корня в iframe оно равно самому iframe.
      scrollHeight:document.body?Math.ceil(document.body.getBoundingClientRect().height):0,
      brokenImages:imgs,
      snens:!!window.snens
    }});
  }
  window.addEventListener("load",done);
  setTimeout(done,400);
  setTimeout(done,1500);
})();
</script>"#;

fn inject(doc: &str, what: &str, html: &str) -> String {
    if what == "head" {
        if let Some(pos) = doc.to_ascii_lowercase().find("<head") {
            if let Some(end) = doc[pos..].find('>') {
                let at = pos + end + 1;
                return format!("{}{html}{}", &doc[..at], &doc[at..]);
            }
        }
        if let Some(pos) = doc.to_ascii_lowercase().find("<html") {
            if let Some(end) = doc[pos..].find('>') {
                let at = pos + end + 1;
                return format!("{}{html}{}", &doc[..at], &doc[at..]);
            }
        }
        return format!("<head>{html}</head>{doc}");
    }
    if let Some(pos) = doc.to_ascii_lowercase().find("<body") {
        if let Some(end) = doc[pos..].find('>') {
            let at = pos + end + 1;
            return format!("{}{html}{}", &doc[..at], &doc[at..]);
        }
    }
    format!("{html}{doc}")
}

/// Документ целиком — как его собирает клиент (`composeDocument`).
pub fn compose(art: &Artifact, lib: &[library::File]) -> String {
    let meta = format!(
        r#"<meta http-equiv="Content-Security-Policy" content="{}">"#,
        csp(art)
    );
    let theme = r#"<style>:root{--paper:#eeeae1;--paper-2:#e4dfd4;--paper-3:#d7d1c3;--ink:#0b0b0b;
--ink-2:#575247;--line:#0b0b0b;--hair:#b8b1a1;--accent:#ff3b00;--accent-ink:#fff6f0;--ok:#14a300;
--mono:"JetBrains Mono",ui-monospace,Menlo,Consolas,monospace;color-scheme:light dark}
@media (prefers-color-scheme:dark){:root{--paper:#0d0d0d;--paper-2:#161616;--paper-3:#1f1f1f;
--ink:#eeeae1;--ink-2:#a8a29a;--line:#eeeae1;--hair:#3a3a3a;--accent:#ff5a26;--accent-ink:#140800}}
html,body{margin:0;padding:0;background:var(--paper);color:var(--ink);font-family:var(--mono);
font-size:13px;line-height:1.45}body{padding:10px 12px;overflow:hidden}</style>"#;
    let scripts = lib
        .iter()
        .map(|f| {
            format!(
                "<script data-genui=\"{}\">{}</script>",
                f.name,
                escape_close(&f.source)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let boot = format!("{SHIM}{scripts}");
    if art.body.to_ascii_lowercase().contains("<html") {
        let doc = inject(&art.body, "head", &format!("{meta}{theme}"));
        return inject(&doc, "start", &boot);
    }
    format!(
        "<!doctype html>\n<html lang=\"ru\"><head><meta charset=\"utf-8\">{meta}{theme}</head>\n\
<body>{boot}\n{}\n</body></html>",
        art.body
    )
}

fn escape_close(s: &str) -> String {
    s.replace("</script", "<\\/script")
}

/// Страница-хост: артефакт живёт в iframe, как у клиента — только так проверка
/// ловит песочницу (CSP, origin `null`, поведение скриптов).
fn host_page(srcdoc: &str) -> String {
    // `</script>` внутри srcdoc закрыл бы скрипт хоста: JSON-строка экранирует
    // кавычки, но не угловые скобки — это отдельный шаг.
    let doc = serde_json::to_string(srcdoc)
        .unwrap_or_else(|_| "\"\"".into())
        .replace("</", "<\\/");
    // Iframe создаётся из JS ПОСЛЕ установки приёмника: когда он был в разметке,
    // быстрый документ успевал отчитаться `ready` в пустоту, и проверка видела
    // «пусто» (гонка, которую поймал живой прогон на простейшем квизе).
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"></head><body>\
<script>\
window.__genui=[];window.__ready=false;\
window.addEventListener('message',function(e){{\
  var f=document.getElementById('a');\
  if(!f||e.source!==f.contentWindow||e.origin!=='null')return;\
  if(!e.data||e.data.snens!==1)return;\
  window.__genui.push(e.data);\
  if(e.data.type==='ready')window.__ready=true;\
}});\
var f=document.createElement('iframe');\
f.id='a';f.setAttribute('sandbox','allow-scripts allow-forms');\
f.setAttribute('srcdoc',{doc});\
f.style.cssText='width:100%;height:900px;border:0';\
f.onload=function(){{window.__loaded=true}};\
document.body.appendChild(f);\
</script></body></html>"
    )
}

// ---------------------------------------------------------------- браузер

/// Прогнать артефакт в браузере. `chrome: None` — найти самим; если браузера
/// нет, вызывающий делает деградацию до lint (эта функция возвращает ошибку).
pub fn check_in_browser(
    art: &Artifact,
    lib: &[library::File],
    chrome: Option<&str>,
    out_dir: Option<&Path>,
    timeout_s: f64,
) -> Result<Report, String> {
    let mut report = Report::default();
    let doc = compose(art, lib);
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_s);
    let mut br = cdp::open(chrome, deadline, timeout_s, out_dir, "genui")?;
    br.set_viewport(1000, 900)?;

    // Родитель слушает доклады песочницы; сама страница лежит файлом, а не
    // data:-URL: у `file://` origin понятнее в отладке, а srcdoc одинаков.
    let tmp = cdp::TempDir::new("genui")?;
    let page = tmp.path().join("host.html");
    let host = host_page(&doc);
    // Отладка проверки: страница-хост сохраняется рядом с кадрами.
    if std::env::var_os("SNENS_GENUI_DEBUG").is_some() {
        let dump = br.out_dir.join("host.html");
        let _ = std::fs::write(&dump, &host);
        eprintln!("genui: страница-хост {}", dump.display());
    }
    std::fs::write(&page, &host).map_err(|e| format!("host.html: {e}"))?;
    let url = format!("file://{}", page.display());
    br.navigate(&url)?;
    br.wait_ready(Duration::from_secs(5)).or_else(|e| {
        if e.contains("readyState") {
            Ok(())
        } else {
            Err(e)
        }
    })?;

    // Окно наблюдения: песочница докладывает `ready` и ошибки через
    // `postMessage`, родитель складывает их в `window.__ready`/`__genui`.
    // Ждём до готовности или до короткого бюджета: висящий документ не должен
    // съедать весь таймаут тула.
    let observe_until = Instant::now() + Duration::from_secs_f64(timeout_s.min(8.0));
    let mut ready = false;
    let mut seen = 0usize;
    while Instant::now() < observe_until {
        if let Ok(v) = br.cdp.eval(&br.session, "window.__ready === true") {
            ready = v.as_bool().unwrap_or(false);
        }
        // Забираем новые доклады: ошибки приходят раньше `ready` и сразу.
        if let Ok(v) = br
            .cdp
            .eval(&br.session, "JSON.stringify(window.__genui||[])")
        {
            if let Some(s) = v.as_str() {
                if let Ok(items) = serde_json::from_str::<Vec<Value>>(s) {
                    for item in items.iter().skip(seen) {
                        match item.get("type").and_then(Value::as_str) {
                            Some("error") => {
                                let msg =
                                    item.get("message").and_then(Value::as_str).unwrap_or("?");
                                let line = item.get("line").and_then(Value::as_u64).unwrap_or(0);
                                report.console.push(format!("строка {line}: {msg}"));
                            }
                            Some("ready") => {
                                report.rendered = item.get("info").cloned();
                            }
                            _ => {}
                        }
                    }
                    seen = items.len();
                }
            }
        }
        if ready {
            break;
        }
        // События CDP копятся в очереди `cdp`: дренируем, чтобы не росла.
        br.cdp.take_events();
        cdp::Browser::sleep(120);
    }
    if !ready {
        report.warn("документ не прислал ready: скрипты не дошли до конца или их нет");
    }
    // Снимок iframe целиком: модель смотрит глазами, а не по числам.
    let selector = "#a";
    let rect = br.cdp.eval(
        &br.session,
        &format!(
            "(() => {{const el=document.querySelector('{selector}');if(!el)return null;\
             const r=el.getBoundingClientRect();return {{x:r.left,y:r.top,w:r.width,h:r.height,\
             dpr:window.devicePixelRatio||1}}}})()"
        ),
    )?;
    if let (Some(x), Some(y), Some(w), Some(h), Some(dpr)) = (
        rect.get("x").and_then(Value::as_f64),
        rect.get("y").and_then(Value::as_f64),
        rect.get("w").and_then(Value::as_f64),
        rect.get("h").and_then(Value::as_f64),
        rect.get("dpr").and_then(Value::as_f64),
    ) {
        let png = br.screenshot(Some(cdp::clip_of(x, y, w, h, dpr)))?;
        let name = format!(
            "genui-{}-{}.png",
            if art.head.id.is_empty() {
                format!("artifact{}", art.index)
            } else {
                art.head.id.clone()
            },
            std::process::id()
        );
        let path = cdp::unique(&br.out_dir, &name);
        std::fs::write(&path, &png).map_err(|e| format!("{}: {e}", path.display()))?;
        cdp::deliver(&path, &png)?;
        report.screenshot = Some(path);
    }
    let notes = report.console.clone();
    for note in notes {
        report.error(note);
    }
    Ok(report)
}
