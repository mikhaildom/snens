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

#[cfg(unix)]
fn set_mode(p: &std::path::Path, mode: Option<u32>) {
    if let Some(m) = mode {
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(m));
    }
}

#[cfg(not(unix))]
fn set_mode(_p: &std::path::Path, _mode: Option<u32>) {}

/// temp в том же каталоге + rename: обрыв на середине не оставит обрезанный файл.
fn atomic_write(path: &str, data: &[u8], mode: Option<u32>) {
    let dir = parent(path).unwrap_or_else(|| std::path::PathBuf::from("."));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".snens-write-{}-{nanos}", std::process::id()));
    std::fs::write(&tmp, data).unwrap_or_else(|e| die(format!("write: {}: {e}", tmp.display())));
    set_mode(&tmp, mode);
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        die(format!("write: {path}: {e}"));
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
        die(format!("write: ожидается ровно один путь, получено {}", rest.len()));
    }
    let path = rest[0].as_str();

    let mut data = Vec::new();
    if std::io::stdin().read_to_end(&mut data).is_err() {
        die("write: не смог прочитать stdin".into());
    }

    let meta = std::fs::metadata(path).ok();
    let existed = meta.as_ref().map(|m| m.is_file()).unwrap_or(false);
    let before = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let mode = meta.as_ref().and_then(mode_of);

    if let Some(dir) = parent(path) {
        let _ = std::fs::create_dir_all(dir);
    }
    if append {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap_or_else(|e| die(format!("write: {path}: {e}")));
        f.write_all(&data)
            .unwrap_or_else(|e| die(format!("write: {path}: {e}")));
    } else {
        atomic_write(path, &data, mode);
    }

    println!(
        "{}",
        serde_json::json!({
            "path": path,
            "created": !existed,
            "overwrote": existed && before > 0 && !append,
            "appended": append,
            "bytes_before": before,
            "bytes": data.len(),
        })
    );
}