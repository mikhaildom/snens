//! Значок snens внутрь exe: ресурс ICON через windres из w64devkit.
//! Без windres exe собирается, просто со стандартным значком.
use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let ico = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("../../../desktop/icons/icon.ico");
    println!("cargo:rerun-if-changed={}", ico.display());
    if !env::var("TARGET").unwrap_or_default().contains("windows") || !ico.is_file() {
        return;
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let rc = out.join("app.rc");
    let obj = out.join("app-res.o");
    let ico_s = ico.display().to_string().replace(std::path::MAIN_SEPARATOR, "/");
    fs::write(&rc, format!("1 ICON \"{ico_s}\"\n")).unwrap();
    let windres = env::var("WINDRES").unwrap_or_else(|_| "windres".into());
    match Command::new(&windres).args(["-O", "coff", "-i"]).arg(&rc).arg("-o").arg(&obj).status() {
        Ok(s) if s.success() => println!("cargo:rustc-link-arg-bins={}", obj.display()),
        _ => println!("cargo:warning=windres не сработал: snens-app.exe будет без значка"),
    }
}
