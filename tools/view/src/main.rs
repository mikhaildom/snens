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

/// Первые `n` байт файла: спецфайл вроде `/dev/zero` не может утащить процесс
/// в бесконечное чтение, и память ограничена размером головы.
fn read_head(p: &Path, n: usize) -> Result<Vec<u8>, std::io::Error> {
    use std::io::Read;
    let f = std::fs::File::open(p)?;
    let mut buf = Vec::with_capacity(n);
    f.take(n as u64).read_to_end(&mut buf)?;
    Ok(buf)
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

/// Расширение по распознанному типу — не по имени источника: ядро выбирает mime
/// вложения по расширению, а файл `photo.png` с JPEG-байтами иначе уехал бы как
/// сломанный PNG.
fn ext_for(mime: &str) -> &'static str {
    match mime {
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        _ => "",
    }
}

/// Положить файл в канал и вернуть путь. Имя занимается атомарно (`create_new`),
/// поэтому два параллельных `view` не выбирают одно и то же имя и не затирают
/// вложение друг друга. Содержимое копируется потоком: файл в память не читаем.
fn place(dir: &Path, src: &Path, mime: &str) -> Result<PathBuf, String> {
    let name = src.file_name().and_then(|n| n.to_str()).unwrap_or("image");
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    let ext = ext_for(mime);
    let mut last = String::new();
    for i in 0..1000 {
        let dest = if i == 0 {
            dir.join(format!("{stem}{ext}"))
        } else {
            dir.join(format!("{stem}-{i}{ext}"))
        };
        let mut out = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dest)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last = dest.display().to_string();
                continue;
            }
            Err(e) => return Err(format!("{}: {e}", dest.display())),
        };
        let mut input = std::fs::File::open(src).map_err(|e| format!("{}: {e}", src.display()))?;
        if let Err(e) = std::io::copy(&mut input, &mut out) {
            // Недописанное вложение модели не нужно: убираем и говорим вслух.
            drop(out);
            let _ = std::fs::remove_file(&dest);
            return Err(format!("{}: {e}", dest.display()));
        }
        return Ok(dest);
    }
    Err(format!("в канале {last}: не нашлось свободного имени"))
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
        // Голова файла, а не весь файл: для sniff и первых полей заголовка хватает
        // килобайт, а гигабайтная картинка в память не влезает. Копирование
        // в канал всё равно прочитает файл ещё раз, потоком.
        let meta = match std::fs::metadata(p) {
            Ok(m) => m,
            Err(e) => die(format!("view: {path}: {e}")),
        };
        if !meta.is_file() {
            die(format!("view: {path} — не обычный файл"));
        }
        let data = read_head(p, 64 * 1024).unwrap_or_else(|e| die(format!("view: {path}: {e}")));
        let Some(mime) = sniff(&data) else {
            die(format!(
                "view: {path} — не изображение (PNG/JPEG/GIF/WebP), {} Б",
                data.len()
            ));
        };
        // Размер — из заголовка; «не разобрал» печатаем явно, а не как 0x0.
        let size = match dims(&data, mime) {
            Some((w, h)) => format!("{w}x{h}"),
            None => "?x?".to_string(),
        };
        match &dir {
            Some(d) => {
                if let Err(e) = std::fs::create_dir_all(d) {
                    die(format!("view: канал вложений {}: {e}", d.display()));
                }
                let to = place(d, p, mime).unwrap_or_else(|e| die(format!("view: {e}")));
                println!(
                    "{}: {} {size} {} Б → вложение {}",
                    path,
                    mime,
                    meta.len(),
                    to.file_name().and_then(|n| n.to_str()).unwrap_or("?")
                );
            }
            None => println!(
                "{}: {} {size} {} Б — канал вложений недоступен (нет SNENS_ATTACH_DIR), \
                 метаданные показаны, но картинка модели не уехала",
                path,
                mime,
                meta.len()
            ),
        }
    }
}
