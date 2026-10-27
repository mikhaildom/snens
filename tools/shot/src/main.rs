// shot — скриншот страницы через headless Chrome по CDP: элемент по CSS-селектору
// (рамка считается в браузере, а не подбирается глазами), явный кроп, несколько
// размеров за один запуск и JS-проба вместо probe-HTML + `chrome --dump-dom`.
//
// Транспорт (запуск хрома, сокет CDP, дедлайн, кадры, канал вложений) живёт в
// крейте `cdp`: тот же обязан видеть страницу `genui check`, иначе проверка
// артефакта врала бы про то, что видит человек.
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

const USAGE: &str = "shot <url> [--size WxH[,WxH...]] [--selector CSS] [--crop X,Y,W,H] [--script файл.js] [--wait-ms N] [--timeout S] [--chrome путь] [--out-dir каталог]";

const HELP: &str = "\
shot — снять скриншот страницы headless-хромом: элемент по селектору, кроп, JS-проба.
shot <url> [--size WxH[,WxH...]] [--selector CSS] [--crop X,Y,W,H] [--script файл.js] [--no-shot] [--wait-ms N] [--timeout S] [--chrome путь] [--out-dir каталог]

Открывает URL в headless Chrome (CDP по ws://127.0.0.1), снимает PNG и кладёт файл
в канал вложений харнесса (SNENS_ATTACH_DIR) — оттуда картинка уезжает в контекст.

  --size WxH         размер окна; несколько через запятую (дефолт 1440x900):
                     `--size 1440x900,390x844` — десктоп и мобильный кадр за запуск
  --selector CSS     снять рамку элемента через CDP и отдать кроп именно его
  --crop X,Y,W,H     явный кроп (CSS-пиксели документа)
  --script файл.js   выполнить JS на странице и напечатать JSON результата.
                     Содержимое файла — тело async-функции: значение возвращается
                     через return, напр. `return {title: document.title}`.
  --no-shot          только JS-проба: картинку не снимать (экономит вложение)
  --wait-ms N        пауза после загрузки, мс (дефолт 300)
  --timeout S        внутренний таймаут всего запуска, секунды (дефолт 20)
  --out-dir каталог  куда класть PNG (дефолт — временный каталог, удаляется)

С `--script` без снимка действует как DOM-проба: probe-HTML в проекте не нужен.
Chrome берётся из --chrome, SNENS_CHROME, ~/.cache/ms-playwright/chromium-*/chrome-linux64/chrome
или PATH (chrome, chrome-headless-shell, chromium).

Коды: 0 — успех, 2 — неверные аргументы, 1 — ошибка выполнения (включая таймаут).";

fn die(msg: &str, code: i32) -> ! {
    eprintln!("{msg}");
    std::process::exit(code);
}

// ---------------------------------------------------------------- аргументы

#[derive(Clone)]
struct Size {
    w: u32,
    h: u32,
}

struct Args {
    url: String,
    sizes: Vec<Size>,
    selector: Option<String>,
    crop: Option<(u32, u32, u32, u32)>,
    script: Option<PathBuf>,
    no_shot: bool,
    wait_ms: u64,
    timeout: f64,
    chrome: Option<String>,
    out_dir: Option<PathBuf>,
}

/// `--size=1440x900` → `["--size", "1440x900"]`: один разбор вместо двух.
fn expand(argv: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for a in argv {
        match a.strip_prefix("--").and_then(|r| r.split_once('=')) {
            Some((k, v)) => {
                out.push(format!("--{k}"));
                out.push(v.to_string());
            }
            None => out.push(a.clone()),
        }
    }
    out
}

fn num<T: std::str::FromStr>(argv: &[String], i: usize, name: &str) -> T {
    let v = argv.get(i + 1).unwrap_or_else(|| {
        die(&format!("shot: {name} без значения\n{USAGE}"), 2);
    });
    v.parse().unwrap_or_else(|_| {
        die(
            &format!("shot: {name} ждёт число, получено {v}\n{USAGE}"),
            2,
        )
    })
}

fn parse_size(s: &str) -> Size {
    let (w, h) = s
        .split_once(['x', 'X', '×'])
        .unwrap_or_else(|| die(&format!("shot: --size ждёт WxH, получено {s}\n{USAGE}"), 2));
    let w: u32 = w
        .parse()
        .unwrap_or_else(|_| die(&format!("shot: --size: ширина не число: {w}\n{USAGE}"), 2));
    let h: u32 = h
        .parse()
        .unwrap_or_else(|_| die(&format!("shot: --size: высота не число: {h}\n{USAGE}"), 2));
    if w == 0 || h == 0 || w > 10_000 || h > 10_000 {
        die(
            &format!("shot: --size: {s} вне разумного (1..10000)\n{USAGE}"),
            2,
        );
    }
    Size { w, h }
}

fn parse_crop(s: &str) -> (u32, u32, u32, u32) {
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        die(
            &format!("shot: --crop ждёт X,Y,W,H, получено {s}\n{USAGE}"),
            2,
        );
    }
    let mut n = [0u32; 4];
    for (i, p) in parts.iter().enumerate() {
        n[i] = p
            .parse()
            .unwrap_or_else(|_| die(&format!("shot: --crop: не число: {p}\n{USAGE}"), 2));
    }
    if n[2] == 0 || n[3] == 0 {
        die(&format!("shot: --crop: нулевые W/H\n{USAGE}"), 2);
    }
    (n[0], n[1], n[2], n[3])
}

fn parse(argv: &[String]) -> Args {
    let argv = expand(argv);
    let mut url: Option<String> = None;
    let mut a = Args {
        url: String::new(),
        sizes: Vec::new(),
        selector: None,
        crop: None,
        script: None,
        no_shot: false,
        wait_ms: 300,
        timeout: 20.0,
        chrome: None,
        out_dir: None,
    };
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        match arg {
            "--no-shot" => {
                a.no_shot = true;
                i += 1;
                continue;
            }
            "--size" | "--selector" | "--crop" | "--script" | "--wait-ms" | "--timeout"
            | "--chrome" | "--out-dir" => {
                // Значение проверяем сразу: пустой --size молчаливого дефолта не заслуживает.
                match arg {
                    "--size" => {
                        let v = argv.get(i + 1).unwrap_or_else(|| {
                            die(&format!("shot: --size без значения\n{USAGE}"), 2)
                        });
                        for s in v.split(',') {
                            if !s.trim().is_empty() {
                                a.sizes.push(parse_size(s.trim()));
                            }
                        }
                        if a.sizes.is_empty() {
                            die(&format!("shot: --size пуст\n{USAGE}"), 2);
                        }
                    }
                    "--selector" => {
                        let v = argv.get(i + 1).unwrap_or_else(|| {
                            die(&format!("shot: --selector без значения\n{USAGE}"), 2)
                        });
                        if v.trim().is_empty() {
                            die(&format!("shot: --selector пуст\n{USAGE}"), 2);
                        }
                        a.selector = Some(v.clone());
                    }
                    "--crop" => {
                        let v = argv.get(i + 1).unwrap_or_else(|| {
                            die(&format!("shot: --crop без значения\n{USAGE}"), 2)
                        });
                        a.crop = Some(parse_crop(v));
                    }
                    "--script" => {
                        let v = argv.get(i + 1).unwrap_or_else(|| {
                            die(&format!("shot: --script без значения\n{USAGE}"), 2)
                        });
                        a.script = Some(PathBuf::from(v));
                    }
                    "--wait-ms" => a.wait_ms = num(&argv, i, "--wait-ms"),
                    "--timeout" => {
                        let t: f64 = num(&argv, i, "--timeout");
                        if t <= 0.0 || t > 600.0 {
                            die(&format!("shot: --timeout вне 0..600 с: {t}\n{USAGE}"), 2);
                        }
                        a.timeout = t;
                    }
                    "--chrome" => a.chrome = Some(argv[i + 1].clone()),
                    _ => a.out_dir = Some(PathBuf::from(&argv[i + 1])),
                }
                i += 2;
            }
            _ if arg.starts_with('-') && arg != "-" => {
                die(&format!("shot: неизвестный аргумент {arg}\n{USAGE}"), 2)
            }
            _ => {
                if url.is_some() {
                    die(&format!("shot: URL указан дважды\n{USAGE}"), 2);
                }
                url = Some(arg.to_string());
                i += 1;
            }
        }
    }
    a.url = url.unwrap_or_else(|| die(&format!("shot: не указан URL\n{USAGE}"), 2));
    if !(a.url.starts_with("http://") || a.url.starts_with("https://")) {
        die(
            &format!("shot: только http/https, получено {}\n{USAGE}", a.url),
            2,
        );
    }
    if a.selector.is_some() && a.crop.is_some() {
        die(
            &format!("shot: --selector и --crop взаимоисключающие — рамку элемента задаёт селектор\n{USAGE}"),
            2,
        );
    }
    if a.no_shot && a.script.is_none() {
        die(
            &format!("shot: --no-shot без --script — снимать нечего\n{USAGE}"),
            2,
        );
    }
    if a.sizes.is_empty() {
        a.sizes.push(Size { w: 1440, h: 900 });
    }
    if let Some(p) = &a.script {
        if !p.is_file() {
            die(
                &format!("shot: --script: нет файла {}\n{USAGE}", p.display()),
                2,
            );
        }
    }
    a
}

// ---------------------------------------------------------------- селектор

#[derive(Clone)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    sx: f64,
    sy: f64,
    vw: f64,
    vh: f64,
    dpr: f64,
    tag: String,
}

fn measure(cdp: &mut cdp::Cdp, session: &str, selector: &str) -> Result<Option<Rect>, String> {
    let sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into());
    let js = format!(
        r#"(() => {{
  let el;
  try {{ el = document.querySelector({sel}); }}
  catch (e) {{ return {{ bad: String(e && e.message || e) }}; }}
  if (!el) return {{ missing: true }};
  const r = el.getBoundingClientRect();
  return {{
    x: r.left, y: r.top, w: r.width, h: r.height,
    sx: window.scrollX, sy: window.scrollY,
    vw: window.innerWidth, vh: window.innerHeight,
    dpr: window.devicePixelRatio || 1,
    tag: String(el.tagName || '').toLowerCase()
  }};
}})()"#
    );
    let v = cdp.eval(session, &js)?;
    if let Some(bad) = v.get("bad").and_then(Value::as_str) {
        return Err(format!("--selector: кривой CSS-селектор {selector}: {bad}"));
    }
    if v.get("missing").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(None);
    }
    let f = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    Ok(Some(Rect {
        x: f("x"),
        y: f("y"),
        w: f("w"),
        h: f("h"),
        sx: f("sx"),
        sy: f("sy"),
        vw: f("vw"),
        vh: f("vh"),
        dpr: if f("dpr") > 0.0 { f("dpr") } else { 1.0 },
        tag: v
            .get("tag")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string(),
    }))
}

/// Элемент может быть за пределами вьюпорта: сдвигаем страницу к нему, чтобы
/// кадр не оказался пустым. Координаты всё равно берём из getBoundingClientRect.
fn shift_to_view(cdp: &mut cdp::Cdp, session: &str, selector: &str) -> Result<Rect, String> {
    let mut last: Option<Rect> = None;
    for _ in 0..4 {
        let Some(r) = measure(cdp, session, selector)? else {
            return Err(format!(
                "--selector: элемента `{selector}` нет на странице (querySelector → null)"
            ));
        };
        let mut dx = 0.0;
        let mut dy = 0.0;
        if r.x < 0.0 {
            dx = r.x;
        } else if r.w <= r.vw && r.x + r.w > r.vw {
            dx = r.x + r.w - r.vw;
        }
        if r.y < 0.0 {
            dy = r.y;
        } else if r.h <= r.vh && r.y + r.h > r.vh {
            dy = r.y + r.h - r.vh;
        }
        let done = dx.abs() < 1.0 && dy.abs() < 1.0;
        last = Some(r);
        if done {
            break;
        }
        cdp.eval(session, &format!("window.scrollBy({dx:.1}, {dy:.1}); 0"))?;
        cdp::Browser::sleep(60);
    }
    last.ok_or_else(|| format!("--selector: не удалось измерить `{selector}`"))
}

// ---------------------------------------------------------------- прогон

fn run(a: &Args) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs_f64(a.timeout);
    // Проба без снимка всё равно требует вьюпорта: устройство по умолчанию в
    // headless — 800x600, и любой layout-зависимый JS соврёт.
    let first = a.sizes[0].clone();
    let mut br = cdp::open(
        a.chrome.as_deref(),
        deadline,
        a.timeout,
        a.out_dir.as_deref(),
        "shot",
    )?;
    br.set_viewport(first.w, first.h)?;

    // Порядок шагов — часть контракта: страница открывается один раз, `--script`
    // выполняется сразу после готовности, и только потом снимается кадр. Иначе
    // сценарий «нажать и показать результат» снимал бы исходный вид: снимок
    // делался до пробы, а повторный `navigate` на второй размер стирал состояние.
    br.navigate(&a.url)?;
    let ready_budget = br.cdp.left()?;
    br.wait_ready(ready_budget)?;
    if a.wait_ms > 0 {
        cdp::Browser::sleep(a.wait_ms);
    }
    if let Some(script) = &a.script {
        probe(&mut br, script)?;
    }

    let sizes: &[Size] = if a.no_shot { &[] } else { &a.sizes };
    for size in sizes {
        br.set_viewport(size.w, size.h)?;
        let (clip, tag) = if let Some(sel) = &a.selector {
            let r = shift_to_view(&mut br.cdp, &br.session, sel)?;
            println!(
                "рамка `{sel}` (<{}>): x={:.0} y={:.0} {}x{} CSS px, dpr {}",
                r.tag,
                r.x + r.sx,
                r.y + r.sy,
                r.w.round() as i64,
                r.h.round() as i64,
                r.dpr
            );
            (
                Some(cdp::clip_of(r.x + r.sx, r.y + r.sy, r.w, r.h, r.dpr)),
                "-sel",
            )
        } else if let Some((cx, cy, cw, ch)) = a.crop {
            let dpr = br
                .cdp
                .eval(&br.session, "window.devicePixelRatio || 1")?
                .as_f64()
                .unwrap_or(1.0);
            let dpr = if dpr > 0.0 { dpr } else { 1.0 };
            (
                Some(cdp::clip_of(
                    cx as f64, cy as f64, cw as f64, ch as f64, dpr,
                )),
                "-crop",
            )
        } else {
            (None, "")
        };
        let png = br.screenshot(clip)?;
        let path = cdp::unique(&br.out_dir, &cdp::shot_name(&a.url, size.w, size.h, tag));
        fs::write(&path, &png).map_err(|e| format!("{}: {e}", path.display()))?;
        cdp::deliver(&path, &png)?;
    }
    Ok(())
}

/// Выполнить `--script` как тело async-функции страницы и напечатать JSON.
fn probe(br: &mut cdp::Browser, script: &Path) -> Result<(), String> {
    let src =
        fs::read_to_string(script).map_err(|e| format!("--script {}: {e}", script.display()))?;
    // Тело async-функции: `return` определяет результат, await разрешён.
    let expr = format!("(async () => {{\n{src}\n}})()");
    let v = br.cdp.eval(&br.session, &expr)?;
    println!(
        "{}",
        serde_json::to_string(&v).unwrap_or_else(|_| "null".into())
    );
    Ok(())
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        println!("{HELP}");
        return;
    }
    if argv.is_empty() {
        die(&format!("shot: не указан URL\n{USAGE}"), 2);
    }
    let a = parse(&argv);
    if let Err(e) = run(&a) {
        eprintln!("shot: {e}");
        std::process::exit(1);
    }
}
