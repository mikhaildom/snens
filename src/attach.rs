// Байты → описание: что за файл лежит в каталоге вложений, бинарный ли поток,
// каковы размеры картинки. Ни спавна, ни сокращения текста — только разбор
// содержимого по заголовкам, без декодирования и без картинных крейтов.
use std::path::{Path, PathBuf};

use crate::log::Part;

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

/// Вложения вызова: файлы whitelisted-типов из каталога канала. Слишком большие
/// отбрасываются с пометкой в тексте — лучше отказ, чем транскрипт с 50 МБ base64.
/// Незнакомые расширения игнорируются: канал не должен превращаться в свалку.
pub fn collect_attachments(dir: &Path, max_bytes: usize, text: &mut String) -> Vec<Part> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}