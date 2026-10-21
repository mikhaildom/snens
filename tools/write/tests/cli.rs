// Сквозные проверки через настоящий бинарник: stdin — как у ядра (тот же контракт),
// поэтому тест ловит и путь чтения stdin, и запись, и форму ответа.
use std::io::Write;
use std::process::{Command, Stdio};

fn run(args: &[&str], stdin: &[u8]) -> (i32, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_write"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("не запустился write");
    child.stdin.as_mut().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn tmp(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("write-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

/// 8 КБ кириллицы: файл обязан совпасть побайтово, а ответ — сообщить обе длины.
#[test]
fn large_cyrillic_content_is_written_verbatim() {
    let mut s = String::new();
    for i in 1..=120 {
        s.push_str(&format!(
            "// Строка {i}: проверка сохранности кириллицы — «ёжик», «объявление», «счётчик».\n"
        ));
    }
    let p = tmp("ok.ts");
    let (rc, out, err) = run(&[p.to_str().unwrap()], s.as_bytes());
    assert_eq!(rc, 0, "stderr: {err}");
    let disk = std::fs::read(&p).unwrap();
    assert_eq!(disk, s.as_bytes(), "файл не совпал с входом");
    let ev: serde_json::Value = serde_json::from_str(out.trim()).expect("ответ не JSON");
    assert_eq!(ev["bytes"].as_u64().unwrap() as usize, s.len());
    assert_eq!(ev["bytes_on_disk"].as_u64().unwrap() as usize, disk.len());
    assert_eq!(ev["lines"].as_u64().unwrap() as u64, s.matches('\n').count() as u64);
}

/// Битый content — отказ, файла нет. Раньше порча молча уезжала на диск.
#[test]
fn corrupt_content_is_refused_and_no_file_written() {
    let p = tmp("bad.ts");
    let bad = "// вып\u{fffd}\u{fffd}лнение\n";
    let (rc, _out, err) = run(&[p.to_str().unwrap()], bad.as_bytes());
    assert_ne!(rc, 0, "битый content принят");
    assert!(err.contains("U+FFFD"), "нет причины отказа: {err}");
    assert!(!p.exists(), "файл записан несмотря на порчу");
}

#[test]
fn append_separates_lines_and_reports_counts() {
    let p = tmp("append.ts");
    let (rc, _o, err) = run(&[p.to_str().unwrap()], "первая\n".as_bytes());
    assert_eq!(rc, 0, "{err}");
    let (rc, out, err) = run(&[p.to_str().unwrap(), "--append"], "вторая\n".as_bytes());
    assert_eq!(rc, 0, "{err}");
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "первая\nвторая\n");
    let ev: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(ev["appended"], true);
    assert_eq!(ev["bytes_on_disk"].as_u64().unwrap(), "первая\nвторая\n".len() as u64);
}
