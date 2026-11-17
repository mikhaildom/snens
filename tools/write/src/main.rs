// write — записать файл целиком, содержимое берётся со stdin.
// Для точечных правок есть edit; здесь — новые файлы и полная перезапись.
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::exit;

const HELP: &str = "\
write — записать файл целиком, содержимое берётся со stdin.
write <путь> [--append]

Для новых файлов и полной перезаписи. Точечные правки — через edit.
Создаёт родительские каталоги, пишет атомарно. В stdout — JSON-событие.";

fn die(msg: String) -> ! {
    eprintln!("{msg}");
    exit(1);
}

fn parent(dir_of: &str) -> Option<std::path::PathBuf> {
    Path::new(dir_of)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> Option<u32> {
    Some(meta.permissions().mode())
}

/// Windows прав файла не знает — сохранять нечего.
#[cfg(not(unix))]
fn mode_of(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

/// Права ставим до записи данных и не глотаем ошибку: молчаливое «0644 вместо
/// 0600» на файле с секретами хуже отказа. `m & 0o7777` — потому что `mode()`
/// несёт ещё и биты типа файла.
#[cfg(unix)]
fn set_mode(p: &std::path::Path, mode: Option<u32>) {
    if let Some(m) = mode {
        let m = m & 0o7777;
        if let Err(e) = std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)) {
            die(format!(
                "write: {}: не удалось сохранить права {:o}: {e}",
                p.display(),
                m
            ));
        }
    }
}

#[cfg(not(unix))]
fn set_mode(_p: &std::path::Path, _mode: Option<u32>) {}

/// Последний байт файла — перевод строки? Нечитаемый файл считаем разделённым:
/// лишняя пустая строка безвредна, склейка — нет.
fn ends_with_newline(path: &str) -> bool {
    match std::fs::File::open(path) {
        Ok(mut f) => {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len == 0 {
                return true;
            }
            if std::io::Seek::seek(&mut f, std::io::SeekFrom::End(-1)).is_err() {
                return true;
            }
            let mut b = [0u8; 1];
            match f.read_exact(&mut b) {
                Ok(()) => b[0] == b'\n',
                Err(_) => true,
            }
        }
        Err(_) => true,
    }
}

/// temp в том же каталоге + rename: обрыв на середине не оставит обрезанный файл.
fn atomic_write(path: &str, data: &[u8], mode: Option<u32>) {
    let dir = parent(path).unwrap_or_else(|| std::path::PathBuf::from("."));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".snens-write-{}-{nanos}", std::process::id()));
    // create_new: имя предсказуемо, и `fs::write` (create+truncate) по симлинку,
    // заранее положенному в каталог, записал бы содержимое в чужой файл.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .unwrap_or_else(|e| die(format!("write: {}: {e}", tmp.display())));
    // Права — до данных: иначе секреты успевают полежать с дефолтными 0644.
    set_mode(&tmp, mode);
    if let Err(e) = f.write_all(data) {
        let _ = std::fs::remove_file(&tmp);
        die(format!("write: {}: {e}", tmp.display()));
    }
    // sync_all до rename: иначе после сбоя питания rename виден, а данных нет —
    // ровно тот обрезанный файл, ради отсутствия которого всё и делается.
    if let Err(e) = f.sync_all() {
        let _ = std::fs::remove_file(&tmp);
        die(format!("write: {}: {e}", tmp.display()));
    }
    drop(f);
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        die(format!("write: {path}: {e}"));
    }
    // И каталог: без fsync самой записи о переименовании на диске может не быть.
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(&dir) {
        let _ = d.sync_all();
    }
}

/// U+FFFD в content — не текст, а след потерянных байт: где-то по пути вход
/// прошёл через lossy-декод. Такой файл писать нельзя: порча молча уедет на диск
/// (тикет 2026-10-21-write-cyrillic-content-corrupt). Сообщаем позицию, а не
/// просто «нельзя».
fn check_replacement(data: &[u8]) -> Result<(), String> {
    let text = match std::str::from_utf8(data) {
        Ok(t) => t,
        Err(e) => {
            return Err(format!(
                "write: stdin не UTF-8 ({e}) — вход побит, файл не записан"
            ))
        }
    };
    // Считаем все, а держим позиции только первых пяти: вход, целиком
    // состоящий из U+FFFD, не должен материализовать вектор на весь файл.
    let total = text.matches('\u{fffd}').count();
    if total == 0 {
        return Ok(());
    }
    let bad: Vec<usize> = text
        .char_indices()
        .filter(|(_, c)| *c == '\u{fffd}')
        .map(|(i, _)| i)
        .take(5)
        .collect();
    let sample: String = text
        .char_indices()
        .filter(|(_, c)| *c == '\u{fffd}')
        .take(1)
        .map(|(i, _)| {
            // Хвост вокруг места порчи: по нему видно, какой символ потерялся.
            let from = text[..i]
                .char_indices()
                .rev()
                .nth(20)
                .map(|(j, _)| j)
                .unwrap_or(0);
            let to = text[i..]
                .char_indices()
                .nth(20)
                .map(|(j, _)| i + j)
                .unwrap_or(text.len());
            format!("{:?}", &text[from..to])
        })
        .collect();
    Err(format!(
        "write: content содержит U+FFFD ({total} шт., позиции {bad:?}) — это след \
         потерянных байт, файл не записан. Место: {sample}. Пересобери содержимое: при \
         кириллице такой длины надёжнее `python3 - <<'PY'` в run"
    ))
}

/// Конечная цель симлинка (цепочка разрешается `canonicalize`), иначе путь как есть.
/// Несуществующий путь или битая ссылка — остаётся как есть: `write` создаёт файл.
fn resolve_symlink(path: &str) -> String {
    let p = Path::new(path);
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => std::fs::canonicalize(p)
            .map(|c| c.display().to_string())
            .unwrap_or_else(|_| path.to_string()),
        _ => path.to_string(),
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "-h" || argv[0] == "--help" {
        println!("{HELP}");
        exit(if argv.is_empty() { 1 } else { 0 });
    }
    let append = argv.iter().any(|a| a == "--append");
    let rest: Vec<&String> = argv.iter().filter(|a| *a != "--append").collect();
    if rest.len() != 1 {
        die(format!(
            "write: ожидается ровно один путь, получено {}",
            rest.len()
        ));
    }
    let path = rest[0].as_str();

    let mut data = Vec::new();
    if std::io::stdin().read_to_end(&mut data).is_err() {
        die("write: не смог прочитать stdin".into());
    }
    if let Err(e) = check_replacement(&data) {
        die(e);
    }

    // Симлинк разрешаем до конца: `metadata` идёт по ссылке, а `rename` заменил бы
    // саму ссылку обычным файлом — конфиг из репо, слинкованный в ~, перестал бы
    // обновляться, а ссылка бы умерла. `--append` пишет сквозь ссылку, и режимы
    // не должны расходиться.
    let path = &resolve_symlink(path)[..];
    let meta = std::fs::metadata(path).ok();
    let existed = meta.as_ref().map(|m| m.is_file()).unwrap_or(false);
    let before = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let mode = meta.as_ref().and_then(mode_of);

    if let Some(dir) = parent(path) {
        // Ошибку не глотаем: иначе она всплывёт из atomic_write как «write: <temp
        // путь>», и настоящая причина (нет прав, компонент — файл) потеряется.
        if let Err(e) = std::fs::create_dir_all(&dir) {
            die(format!("write: {}: {e}", dir.display()));
        }
    }
    let mut separated = false;
    if append {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap_or_else(|e| die(format!("write: {path}: {e}")));
        // Хвост без перевода строки + блок без него = склейка строк
        // (`}/// комментарий`), которую видно только сборкой. Разделитель
        // дописываем сами — вызывающему не надо про него помнить.
        // Пустой вход ничего не пишет и не должен менять файл: иначе `--append`
        // без данных дописывал синтетический перевод строки.
        if !data.is_empty() && before > 0 && !data.starts_with(b"\n") && !ends_with_newline(path) {
            f.write_all(b"\n")
                .unwrap_or_else(|e| die(format!("write: {path}: {e}")));
            separated = true;
        }
        f.write_all(&data)
            .unwrap_or_else(|e| die(format!("write: {path}: {e}")));
    } else {
        atomic_write(path, &data, mode);
    }

    // Сверка с тем, что реально легло на диск: длина и число строк. Расхождение —
    // не «мелочь»: это либо обрыв записи, либо чужая правка между нами и файлом.
    let on_disk = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => die(format!("write: {path}: не читается после записи: {e}")),
    };
    let expect = if append {
        before as usize + usize::from(separated) + data.len()
    } else {
        data.len()
    };
    if on_disk.len() != expect {
        die(format!(
            "write: {path}: на диске {} байт, ожидалось {expect} — запись не совпала с входом",
            on_disk.len()
        ));
    }
    let lines_in = data.iter().filter(|b| **b == b'\n').count();
    let lines_disk = on_disk.iter().filter(|b| **b == b'\n').count();
    if lines_in != lines_disk && !append {
        die(format!(
            "write: {path}: строк на диске {lines_disk}, во входе {lines_in} — содержимое искажено"
        ));
    }

    println!(
        "{}",
        serde_json::json!({
            "path": path,
            "created": !existed,
            "overwrote": existed && before > 0 && !append,
            "appended": append,
            "separator_added": separated,
            "bytes_before": before,
            "bytes": data.len(),
            "bytes_on_disk": on_disk.len(),
            "lines": lines_disk,
            "lines_in": lines_in,
        })
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Наблюдённый баг: битый content (U+FFFD вместо кириллицы) уезжал в файл
    /// молча. Теперь отказ, и в сообщении видно место — иначе диагностика
    /// съедает лишние вызовы.
    #[test]
    fn replacement_char_is_rejected_with_position() {
        let e = check_replacement("ab\u{fffd}cd".as_bytes()).unwrap_err();
        assert!(e.contains("U+FFFD"), "{e}");
        assert!(e.contains("[2]"), "нет позиции: {e}");
        assert!(e.contains("файл не записан"), "{e}");
    }

    #[test]
    fn clean_cyrillic_passes() {
        let s = "// выпълнение: ёжик, объявление, счётчик\n".repeat(500);
        assert!(check_replacement(s.as_bytes()).is_ok());
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let e = check_replacement(b"ok \xd1").unwrap_err();
        assert!(e.contains("не UTF-8"), "{e}");
    }
}
