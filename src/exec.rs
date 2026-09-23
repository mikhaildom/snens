// Выполнение процесса: единственный механизм и для `run`, и для внешних тулов.
// Таймаут, кап вывода, закрытый stdin, exit code.
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::{log::Part, R};

/// Строку такой длины модель всё равно не читает, а контекст она съедает.
const MAX_LINE: usize = 400;

pub struct Out {
    pub exit: i32,
    pub text: String,
    pub bytes: usize,
    pub ms: u64,
    pub timed_out: bool,
    /// Вложения, принесённые процессом через `SNENS_ATTACH_DIR`.
    pub parts: Vec<Part>,
}

pub struct Spawn<'a> {
    /// Путь к исполняемому файлу или имя программы: и тул, и оболочка — процесс.
    pub program: String,
    pub args: Vec<String>,
    /// Что подать на stdin. None — stdin закрывается сразу (иначе тул, читающий
    /// stdin, будет ждать до таймаута).
    pub stdin: Option<String>,
    pub cwd: Option<&'a str>,
    pub timeout_s: u64,
    pub cap: usize,
    /// PATH дочернего процесса: тулы из реестра видны и внутри `run`.
    pub path: Option<&'a str>,
    /// Превентивное сокращение вывода (ANSI, длинные строки, повторы).
    /// Для shell-команд — да, для тулов с точным контрактом (read/edit) — нет:
    /// там схлопывание повторов исказило бы содержимое файла.
    pub reduce: bool,
    /// Куда положить полный вывод, если он не влез в cap. Модель получает путь
    /// и может дочитать или grep'нуть вместо повторного запуска команды.
    pub dump_dir: Option<&'a Path>,
    /// Каталог вложений вызова: если задан, процесс получает его в
    /// `SNENS_ATTACH_DIR` и может оставить там файлы. Ядро подберёт их после
    /// выхода и вернёт как `Out::parts` — одинаково для тулов и для `run`.
    pub attach_dir: Option<&'a Path>,
    /// Сколько байт вложения допускаем; больше — файл отбрасывается с пометкой.
    pub max_attachment_bytes: usize,
}

impl<'a> Spawn<'a> {
    /// Команда строкой через оболочку из конфига: на unix `sh -c`, на Windows
    /// `cmd /C`. Платформа в коде не зашита, и `sh` ищется в PATH — на Android
    /// он в /system/bin, а не в /bin.
    pub fn sh(shell: &[String], cmd: &str, timeout_s: u64, cap: usize) -> Self {
        let mut args: Vec<String> = shell.iter().skip(1).cloned().collect();
        args.push(cmd.to_string());
        Spawn {
            program: shell.first().cloned().unwrap_or_else(|| "sh".into()),
            args,
            stdin: None,
            cwd: None,
            timeout_s,
            cap,
            path: None,
            reduce: false,
            dump_dir: None,
            attach_dir: None,
            max_attachment_bytes: MAX_ATTACHMENT_BYTES,
        }
    }

    pub fn argv(program: &str, args: Vec<String>) -> Self {
        Spawn {
            program: program.to_string(),
            args,
            stdin: None,
            cwd: None,
            timeout_s: 120,
            cap: 40_000,
            path: None,
            reduce: false,
            dump_dir: None,
            attach_dir: None,
            max_attachment_bytes: MAX_ATTACHMENT_BYTES,
        }
    }
}

/// Потолок вложения: больше — это уже не картинка «посмотреть», а файл для инструмента.
/// Задаётся в конфиге, здесь — только дефолт для прямых вызовов `Spawn`.
pub const MAX_ATTACHMENT_BYTES: usize = 32 * 1024 * 1024;

/// Что именно считается вложением — по расширению файла (mime выводится из него).
/// Набор и порядок — часть контракта канала, поэтому он один, а не в каждом туле.
const ATTACH_EXTS: &[(&str, &str, &str)] = &[
    ("png", "image", "image/png"),
    ("jpg", "image", "image/jpeg"),
    ("jpeg", "image", "image/jpeg"),
    ("gif", "image", "image/gif"),
    ("webp", "image", "image/webp"),
    ("mp3", "audio", "audio/mpeg"),
    ("wav", "audio", "audio/wav"),
    ("ogg", "audio", "audio/ogg"),
    ("mp4", "video", "video/mp4"),
    ("webm", "video", "video/webm"),
    ("pdf", "file", "application/pdf"),
];

fn attach_kind(path: &Path) -> Option<(&'static str, &'static str)> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    ATTACH_EXTS
        .iter()
        .find(|(e, _, _)| *e == ext)
        .map(|(_, k, m)| (*k, *m))
}

pub async fn spawn(sp: Spawn<'_>) -> R<Out> {
    let started = Instant::now();
    let mut c = Command::new(&sp.program);
    c.args(&sp.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Процесс убивается вместе с future: таймаут и Ctrl-C без доп. механики.
        .kill_on_drop(true);
    if let Some(d) = sp.cwd {
        c.current_dir(d);
    }
    if let Some(p) = sp.path {
        c.env("PATH", p);
    }
    // Каталог вложений: переменная сама сообщает процессу, что канал есть, и
    // сразу исключает мусор — команда, не знающая про неё, ничего туда не кладёт.
    if let Some(d) = sp.attach_dir {
        std::fs::create_dir_all(d)?;
        c.env("SNENS_ATTACH_DIR", d);
    }

    let mut child = c.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    match sp.stdin {
        // Пишем в отдельной задаче: иначе дедлок, если ребёнок ждёт конца stdin,
        // а мы ждём его вывода.
        Some(input) => {
            if let Some(mut si) = child.stdin.take() {
                tokio::spawn(async move {
                    let _ = si.write_all(input.as_bytes()).await;
                    let _ = si.shutdown().await;
                });
            }
        }
        None => drop(child.stdin.take()),
    }

    let res = tokio::time::timeout(
        Duration::from_secs(sp.timeout_s.max(1)),
        child.wait_with_output(),
    )
    .await;
    let ms = started.elapsed().as_millis() as u64;

    let (exit, stdout, stderr, timed_out) = match res {
        Ok(Ok(out)) => (out.status.code().unwrap_or(-1), out.stdout, out.stderr, false),
        Ok(Err(e)) => return Err(format!("spawn failed: {e}").into()),
        Err(_) => (
            124,
            Vec::new(),
            format!("[timeout after {}s]", sp.timeout_s).into_bytes(),
            true,
        ),
    };

    // Полный вывод (то, что уйдёт в дамп) — байты как есть, до замены бинарных
    // потоков дескриптором: иначе в дампе был бы текст-заглушка, а не оригинал.
    let mut raw_bytes = stdout.clone();
    if !stderr.is_empty() {
        raw_bytes.extend_from_slice(b"\n[stderr]\n");
        raw_bytes.extend_from_slice(&stderr);
    }
    let bytes = raw_bytes.len();
    let (assembled, binary) = join_streams(&stdout, &stderr);
    let mut text = if binary {
        // Дескриптор уже короткий: reduce/truncate к нему неприменимы.
        assembled
    } else if sp.reduce {
        reduce(&assembled, sp.cap)
    } else {
        truncate(&assembled, sp.cap)
    };
    // Обрезанное не теряется: полный вывод ложится в файл, модель получает путь.
    // Бинарный вывод выносим в дамп всегда — в контексте ему места нет, а посмотреть
    // оригинал (hexdump, file) модели может понадобиться.
    if bytes > sp.cap || binary {
        if let Some(dir) = sp.dump_dir {
            if let Ok(p) = dump(dir, &raw_bytes) {
                text.push_str(&format!("\n[полный вывод: {}]", p.display()));
            }
        }
    }

    let parts = match sp.attach_dir {
        Some(d) => collect_attachments(d, sp.max_attachment_bytes, &mut text),
        None => Vec::new(),
    };

    Ok(Out {
        exit,
        text,
        bytes,
        ms,
        timed_out,
        parts,
    })
}

/// Вложения вызова: файлы whitelisted-типов из каталога канала. Слишком большие
/// отбрасываются с пометкой в тексте — лучше отказ, чем транскрипт с 50 МБ base64.
/// Незнакомые расширения игнорируются: канал не должен превращаться в свалку.
fn collect_attachments(dir: &Path, max_bytes: usize, text: &mut String) -> Vec<Part> {
    let mut parts = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return parts;
    };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    // Порядок чтения каталога произволен — сортируем, иначе проекция недетерминирована.
    files.sort();
    for p in files {
        let Some((kind, mime)) = attach_kind(&p) else {
            continue;
        };
        let Ok(meta) = p.metadata() else { continue };
        if meta.len() as usize > max_bytes {
            text.push_str(&format!(
                "\n[вложение отброшено: {} Б > потолка {} Б — {}]",
                meta.len(),
                max_bytes,
                p.file_name().and_then(|n| n.to_str()).unwrap_or("?")
            ));
            continue;
        }
        let (width, height) = if kind == "image" {
            std::fs::read(&p)
                .ok()
                .map(|b| image_dims(&b, mime))
                .unwrap_or((0, 0))
        } else {
            (0, 0)
        };
        if kind == "image" && width == 0 {
            // Не распознали как картинку — не отправляем: провайдер ответит 400.
            text.push_str(&format!(
                "\n[вложение не распознано как изображение: {}]",
                p.file_name().and_then(|n| n.to_str()).unwrap_or("?")
            ));
            continue;
        }
        parts.push(Part {
            kind: kind.to_string(),
            path: p.to_string_lossy().into_owned(),
            mime: mime.to_string(),
            bytes: meta.len() as usize,
            width,
            height,
        });
    }
    parts
}

/// Размеры картинки из заголовка: без декодирования форматы и без картинных крейтов
/// (ядро тащит только std + rustls + serde). Не распознали — (0, 0), и вызывающий решает.
fn image_dims(b: &[u8], mime: &str) -> (u32, u32) {
    match mime {
        "image/png" if b.len() >= 24 && b.starts_with(b"\x89PNG\r\n\x1a\n") => (
            u32::from_be_bytes(b[16..20].try_into().unwrap()),
            u32::from_be_bytes(b[20..24].try_into().unwrap()),
        ),
        "image/gif" if b.len() >= 10 && (b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) => (
            u16::from_le_bytes(b[6..8].try_into().unwrap()) as u32,
            u16::from_le_bytes(b[8..10].try_into().unwrap()) as u32,
        ),
        "image/jpeg" => jpeg_dims(b),
        "image/webp" if b.len() >= 30 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" => {
            match &b[12..16] {
                b"VP8 " => (
                    (u16::from_le_bytes(b[26..28].try_into().unwrap()) & 0x3fff) as u32,
                    (u16::from_le_bytes(b[28..30].try_into().unwrap()) & 0x3fff) as u32,
                ),
                b"VP8L" => {
                    let bits = u32::from_le_bytes(b[21..25].try_into().unwrap());
                    ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
                }
                b"VP8X" => (
                    (u32::from_le_bytes([b[24], b[25], b[26], 0]) & 0xffffff) + 1,
                    (u32::from_le_bytes([b[27], b[28], b[29], 0]) & 0xffffff) + 1,
                ),
                _ => (0, 0),
            }
        }
        _ => (0, 0),
    }
}

/// Рамки JPEG по маркерам: идём по сегментам до SOF (кроме SOF4/SOF8/SOF12).
fn jpeg_dims(b: &[u8]) -> (u32, u32) {
    let mut i = 2; // пропускаем SOI
    while i + 9 < b.len() {
        if b[i] != 0xff {
            i += 1;
            continue;
        }
        let marker = b[i + 1];
        // Паддинг из нескольких 0xff.
        if marker == 0xff {
            i += 1;
            continue;
        }
        // Сегменты без длины.
        if marker == 0x01 || (0xd0..=0xd9).contains(&marker) {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        // SOF0..SOF15, исключая DHT(0xc4), JPG(0xc8), DAC(0xcc).
        if (0xc0..=0xcf).contains(&marker) && marker != 0xc4 && marker != 0xc8 && marker != 0xcc {
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
            return (w, h);
        }
        i += 2 + len;
    }
    (0, 0)
}

/// Бинарный ли поток: решает NUL-байт в первых килобайтах (классический детект),
/// а если NUL нет — доля управляющих байт (кроме таба/переводов строк). Высокие
/// байты (>0x80) сюда не входят: текст в другой кодировке (cp1251, latin-1) ими
/// полон, а бинарём не является.
fn is_binary(b: &[u8]) -> bool {
    let head = &b[..b.len().min(8192)];
    if head.contains(&0) {
        return true;
    }
    if head.is_empty() {
        return false;
    }
    let ctl = head
        .iter()
        .filter(|c| (**c < 0x20 && !matches!(**c, b'\t' | b'\n' | b'\r')) || **c == 0x7f)
        .count();
    ctl * 100 / head.len() > 15
}

/// Дескриптор вместо содержимого: размер и угаданный тип. Содержимого нет даже
/// куском — это hexdump без смысла. Полный вывод вызывающий кладёт в дамп, и
/// модель берёт путь (`file`, `xxd`, `strings` по нему работают).
fn binary_descriptor(bytes: &[u8]) -> String {
    format!("[бинарный вывод, {} Б, {}]", bytes.len(), sniff_mime(bytes))
}

/// Тип бинарного потока по магическим байтам — ровно настолько, чтобы отличить
/// ELF от архива: полноценный mime-детект здесь не нужен.
fn sniff_mime(b: &[u8]) -> &'static str {
    match b {
        _ if b.starts_with(b"\x7fELF") => "ELF",
        _ if b.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) || b.starts_with(&[0xce, 0xfa, 0xed, 0xfe]) => {
            "Mach-O"
        }
        _ if b.starts_with(b"MZ") => "PE/DOS",
        _ if b.starts_with(&[0x1f, 0x8b]) => "gzip",
        _ if b.starts_with(b"PK\x03\x04") => "zip",
        _ if b.starts_with(b"\x89PNG") => "PNG",
        _ if b.starts_with(b"\xff\xd8\xff") => "JPEG",
        _ if b.starts_with(b"%PDF") => "PDF",
        _ => "application/octet-stream",
    }
}

/// Склеить stdout и stderr в одно сообщение модели. Если хоть один поток бинарный,
/// содержимое не отдаём — только дескрипторы (модель и так не прочитает hexdump,
/// а контекст он съедает). Текстовые потоки проходят как раньше.
pub fn join_streams(stdout: &[u8], stderr: &[u8]) -> (String, bool) {
    let out_bin = is_binary(stdout);
    let err_bin = is_binary(stderr);
    let mut s = if out_bin {
        binary_descriptor(stdout)
    } else {
        String::from_utf8_lossy(stdout).into_owned()
    };
    if !stderr.is_empty() {
        s.push_str("\n[stderr]\n");
        if err_bin {
            s.push_str(&binary_descriptor(stderr));
        } else {
            s.push_str(&String::from_utf8_lossy(stderr));
        }
    }
    (s, out_bin || err_bin)
}

/// Полный вывод на диск; имя — от содержимого, поэтому одинаковые дампы не плодятся.
fn dump(dir: &Path, text: &[u8]) -> R<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    let path = dir.join(format!("{:016x}.txt", h.finish()));
    if !path.exists() {
        std::fs::write(&path, text)?;
    }
    Ok(path)
}

/// Превентивное сокращение вывода shell-команд до попадания в контекст:
/// ANSI-мусор, нечеловечески длинные строки, повторы, общий размер.
pub fn reduce(s: &str, cap: usize) -> String {
    let stripped = strip_ansi(s);
    let mut lines: Vec<String> = Vec::new();
    let mut prev: Option<String> = None;
    let mut run = 0usize;
    for raw_line in stripped.lines() {
        let line = cap_line(raw_line.trim_end());
        if Some(&line) == prev.as_ref() {
            run += 1;
            continue;
        }
        if run > 0 {
            lines.push(format!("  [×{}]", run + 1));
            run = 0;
        }
        prev = Some(line.clone());
        lines.push(line);
    }
    if run > 0 {
        lines.push(format!("  [×{}]", run + 1));
    }
    truncate(&lines.join("\n"), cap)
}

fn cap_line(l: &str) -> String {
    let n = l.chars().count();
    if n <= MAX_LINE {
        return l.to_string();
    }
    let head: String = l.chars().take(MAX_LINE).collect();
    format!("{head}… (+{} симв.)", n - MAX_LINE)
}

/// Управляющие последовательности: цвет, прогресс-бары, курсор — токены без смысла.
fn strip_ansi(s: &str) -> String {
    if !s.contains('\u{1b}') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match it.peek() {
            // CSI: ESC [ ... финальный байт в диапазоне @..~
            Some('[') => {
                it.next();
                while let Some(&n) = it.peek() {
                    it.next();
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            }
            // OSC: ESC ] ... BEL или ESC \
            Some(']') => {
                it.next();
                while let Some(n) = it.next() {
                    if n == '\u{7}' {
                        break;
                    }
                    if n == '\u{1b}' && it.peek() == Some(&'\\') {
                        it.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Усечение с явной пометкой: модель должна знать, что видит часть.
pub fn truncate(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let head: String = s.chars().take(cap * 3 / 4).collect();
    let tail: String = s
        .chars()
        .rev()
        .take(cap / 4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!(
        "{head}\n[... обрезано {} символов из {} ...]\n{tail}",
        s.len() - cap,
        s.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_and_repeats_are_squeezed() {
        let s = "\u{1b}[32mok\u{1b}[0m\nшум\nшум\nшум\nконец\n";
        let r = reduce(s, 10_000);
        assert!(r.contains("ok"), "текст потерян: {r}");
        assert!(!r.contains('\u{1b}'), "ANSI остался: {r:?}");
        assert!(r.contains("[×3]"), "повторы не схлопнуты: {r}");
        assert!(r.contains("конец"));
        let long = "x".repeat(1000);
        let r2 = reduce(&long, 10_000);
        assert!(
            r2.contains("симв."),
            "длинная строка без пометки: {}",
            r2.len()
        );
    }

    /// Канал вложений: процесс получает SNENS_ATTACH_DIR и кладёт туда картинку,
    /// ядро подбирает её как Part. Незнакомые типы игнорируются, большие —
    /// отбрасываются с пометкой.
    #[tokio::test]
    async fn attach_dir_collects_images() {
        let dir = std::env::temp_dir().join(format!("snens-attach-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Пишем валидный PNG-заголовок и мусорный файл.
        let png = dir.join("bad-source");
        std::fs::write(
            &png,
            b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01",
        )
        .unwrap();
        let sh = format!(
            "cp '{}' \"$SNENS_ATTACH_DIR/pic.png\"; printf 'x' > \"$SNENS_ATTACH_DIR/note.txt\"",
            png.display()
        );
        let mut sp = Spawn::sh(&["sh".into(), "-c".into()], &sh, 10, 10_000);
        sp.attach_dir = Some(dir.as_path());
        let out = spawn(sp).await.expect("спавн упал");
        assert_eq!(
            out.parts.len(),
            1,
            "должна быть одна картинка: {:?}",
            out.parts
        );
        assert_eq!(out.parts[0].mime, "image/png");
        assert_eq!(out.parts[0].kind, "image");
        assert_eq!((out.parts[0].width, out.parts[0].height), (1, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Потолок вложения: слишком большой файл не едет, но и не теряется молча.
    #[tokio::test]
    async fn oversized_attachment_is_reported() {
        let dir = std::env::temp_dir().join(format!("snens-attach-big-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut sp = Spawn::sh(
            &["sh".into(), "-c".into()],
            "head -c 5000 /dev/zero > \"$SNENS_ATTACH_DIR/big.png\"",
            10,
            10_000,
        );
        sp.attach_dir = Some(dir.as_path());
        sp.max_attachment_bytes = 1000;
        let out = spawn(sp).await.expect("спавн упал");
        assert!(out.parts.is_empty(), "большой файл не должен уехать");
        assert!(
            out.text.contains("вложение отброшено"),
            "нет пометки: {}",
            out.text
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Бинарный stdout не уезжает модели содержимым: только дескриптор с типом
    /// и размером. Текст с редкими невалидными байтами (другая кодировка) — не бинарник.
    #[test]
    fn binary_stdout_becomes_descriptor() {
        let elf = [b"\x7fELF".as_ref(), &[0u8; 32]].concat();
        let (s, bin) = join_streams(&elf, b"");
        assert!(bin, "ELF не распознан бинарным");
        assert!(s.contains("бинарный вывод"), "нет дескриптора: {s}");
        assert!(s.contains("ELF"), "тип не угадан: {s}");
        // UTF-8-текст проходит как есть.
        let (s2, bin2) = join_streams("привет\n".as_bytes(), b"");
        assert!(!bin2 && s2.contains("привет"), "текст испорчен: {s2}");
        // Бинарный stderr — тоже дескриптор.
        let (s3, bin3) = join_streams(b"ok\n", &[0u8, 1, 2, 3]);
        assert!(bin3, "бинарный stderr не пойман");
        assert!(s3.contains("[stderr]") && s3.contains("бинарный вывод"), "{s3}");
    }

    /// Обрезанное не теряется: полный вывод лежит в файле, путь уходит модели.
    #[tokio::test]
    async fn big_output_is_dumped_to_file() {
        let dir = std::env::temp_dir().join(format!("snens-dump-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut sp = Spawn::sh(&["sh".into(), "-c".into()], "seq 1 20000", 20, 500);
        sp.reduce = true;
        sp.dump_dir = Some(dir.as_path());
        let out = spawn(sp).await.expect("спавн упал");

        assert!(out.bytes > 500, "вывод должен быть больше капа");
        assert!(
            out.text.contains("полный вывод: "),
            "нет ссылки на дамп: {}",
            &out.text[..200.min(out.text.len())]
        );
        let files: Vec<_> = std::fs::read_dir(&dir)
            .expect("каталог дампов не создан")
            .flatten()
            .collect();
        assert_eq!(files.len(), 1, "ожидался один дамп, их {}", files.len());
        let full = std::fs::read_to_string(files[0].path()).expect("дамп не читается");
        assert_eq!(full.lines().count(), 20000, "дамп обрезан");
        std::fs::remove_dir_all(&dir).ok();
    }
}
