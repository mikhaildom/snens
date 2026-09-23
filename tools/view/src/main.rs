// view — показать модели изображение с диска: положить файл в канал вложений
// харнесса (`SNENS_ATTACH_DIR`), чтобы он уехал в контекст как image-часть.
// Текст модель читает через read/run; картинку текстом не передать.
use std::path::{Path, PathBuf};
use std::process::exit;

const HELP: &str = "\
view — показать модели изображение с диска (PNG/JPEG/GIF/WebP).
view <путь> [<путь>...]

Картинка уезжает в контекст как изображение, а не текстом. Без канала вложений
(переменная SNENS_ATTACH_DIR) view только печатает метаданные.";

fn die(msg: String) -> ! {
    eprintln!("{msg}");
    exit(1);
}

/// Тип по магии, а не по расширению: провайдер отвергнет файл, названный .png,
/// но не являющийся картинкой.
fn sniff(b: &[u8]) -> Option<&'static str> {
    if b.len() >= 8 && b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.len() >= 3 && b.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if b.len() >= 6 && (b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) {
        Some("image/gif")
    } else if b.len() >= 12 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn dims(b: &[u8], mime: &str) -> Option<(u32, u32)> {
    match mime {
        "image/png" if b.len() >= 24 => Some((
            u32::from_be_bytes(b[16..20].try_into().unwrap()),
            u32::from_be_bytes(b[20..24].try_into().unwrap()),
        )),
        "image/gif" if b.len() >= 10 => Some((
            u16::from_le_bytes(b[6..8].try_into().unwrap()) as u32,
            u16::from_le_bytes(b[8..10].try_into().unwrap()) as u32,
        )),
        "image/jpeg" => jpeg_dims(b),
        "image/webp" if b.len() >= 30 => match &b[12..16] {
            b"VP8 " => Some((
                (u16::from_le_bytes(b[26..28].try_into().unwrap()) & 0x3fff) as u32,
                (u16::from_le_bytes(b[28..30].try_into().unwrap()) & 0x3fff) as u32,
            )),
            b"VP8L" => {
                let bits = u32::from_le_bytes(b[21..25].try_into().unwrap());
                Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
            }
            b"VP8X" => Some((
                u32::from_le_bytes([b[24], b[25], b[26], 0]) + 1,
                u32::from_le_bytes([b[27], b[28], b[29], 0]) + 1,
            )),
            _ => None,
        },
        _ => None,
    }
}

/// Рамки JPEG по маркерам: идём до SOF, кроме DHT/JPG/DAC.
fn jpeg_dims(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 9 < b.len() {
        if b[i] != 0xff {
            i += 1;
            continue;
        }
        let marker = b[i + 1];
        if marker == 0xff {
            i += 1;
            continue;
        }
        if marker == 0x01 || (0xd0..=0xd9).contains(&marker) {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        if (0xc0..=0xcf).contains(&marker) && marker != 0xc4 && marker != 0xc8 && marker != 0xcc {
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

/// Имя в канале: базовое имя источника, при коллизии — с индексом. Расширение
/// сохраняем: по нему ядро выбирает mime вложения.
fn dest(dir: &Path, src: &Path) -> PathBuf {
    let name = src
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("image");
    let mut dest = dir.join(name);
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let mut i = 1;
    while dest.exists() {
        dest = dir.join(format!("{stem}-{i}{ext}"));
        i += 1;
    }
    dest
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "-h" || argv[0] == "--help" {
        println!("{HELP}");
        exit(if argv.is_empty() { 1 } else { 0 });
    }
    let dir = std::env::var_os("SNENS_ATTACH_DIR").map(PathBuf::from);

    for path in &argv {
        let p = Path::new(path);
        let data = match std::fs::read(p) {
            Ok(d) => d,
            Err(e) => die(format!("view: {path}: {e}")),
        };
        let Some(mime) = sniff(&data) else {
            die(format!(
                "view: {path} — не изображение (PNG/JPEG/GIF/WebP), {} Б",
                data.len()
            ));
        };
        let (w, h) = dims(&data, mime).unwrap_or((0, 0));
        match &dir {
            Some(d) => {
                if let Err(e) = std::fs::create_dir_all(d) {
                    die(format!("view: канал вложений {}: {e}", d.display()));
                }
                let to = dest(d, p);
                if let Err(e) = std::fs::copy(p, &to) {
                    die(format!("view: {}: {e}", to.display()));
                }
                println!(
                    "{}: {} {}x{} {} Б → вложение {}",
                    path,
                    mime,
                    w,
                    h,
                    data.len(),
                    to.file_name().and_then(|n| n.to_str()).unwrap_or("?")
                );
            }
            None => println!(
                "{}: {} {}x{} {} Б — канал вложений недоступен (нет SNENS_ATTACH_DIR), \
                 метаданные показаны, но картинка модели не уехала",
                path,
                mime,
                w,
                h,
                data.len()
            ),
        }
    }
}