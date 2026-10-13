//! Обходная работа для тулчейна x86_64-pc-windows-gnu: webview2-com-sys линкуется
//! через импорт-библиотеку, поэтому WebView2Loader.dll обязан лежать рядом с exe
//! (см. tauri#4319). На msvc загрузка статическая и скрипт ничего не делает.
//!
//! DLL берётся из распакованного пакета webview2-com-sys в cargo-реестре.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() != "gnu" {
        return;
    }
    // target/<profile>/ — на три уровня выше OUT_DIR (.../build/<pkg>-<hash>/out)
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let Some(profile_dir) = Path::new(&out_dir).ancestors().nth(3) else {
        return;
    };
    let dest = profile_dir.join("WebView2Loader.dll");
    if dest.exists() {
        return;
    }

    let cargo_home = std::env::var("CARGO_HOME").unwrap_or_default();
    let src_root = Path::new(&cargo_home).join("registry").join("src");
    for idx in std::fs::read_dir(&src_root).into_iter().flatten().flatten() {
        for pkg in std::fs::read_dir(idx.path()).into_iter().flatten().flatten() {
            let name = pkg.file_name().to_string_lossy().into_owned();
            if !name.starts_with("webview2-com-sys-") {
                continue;
            }
            let dll = pkg.path().join("x64").join("WebView2Loader.dll");
            if dll.is_file() {
                if std::fs::copy(&dll, &dest).is_ok() {
                    println!("cargo:warning=WebView2Loader.dll скопирован в {}", profile_dir.display());
                }
                return;
            }
        }
    }
    println!("cargo:warning=WebView2Loader.dll не найден в реестре cargo — окно WebView2 не запустится");
    let _ = PathBuf::new();
}
