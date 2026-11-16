//! Обходная работа для тулчейна x86_64-pc-windows-gnu: webview2-com-sys линкуется
//! через импорт-библиотеку, поэтому WebView2Loader.dll обязан лежать рядом с exe
//! (см. tauri#4319). На msvc загрузка статическая и скрипт ничего не делает.
//!
//! DLL берётся из распакованного пакета webview2-com-sys в cargo-реестре.
//! Переменную CARGO_HOME cargo передаёт сборочному скрипту только если она
//! задана в окружении, а на GitHub-раннерах (и в части локальных установок
//! rustup) она не выставлена — поэтому перебираем ВСЕ стандартные варианты
//! дома cargo: CARGO_HOME, USERPROFILE\.cargo, HOME\.cargo.
//!
//! Если DLL не найден — паника с перечислением просмотренных путей. Раньше
//! скрипт ограничивался cargo:warning: сборка «успешно» проходила, и CI
//! падал позже — на Copy-Item в шаге упаковки, без внятной диагностики.

use std::path::{Path, PathBuf};

/// Все кандидаты в «дом cargo», где лежит registry/src с распакованными пакетами.
fn cargo_homes() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(h) = std::env::var("CARGO_HOME") {
        if !h.is_empty() {
            out.push(PathBuf::from(h));
        }
    }
    for key in ["USERPROFILE", "HOME"] {
        if let Ok(h) = std::env::var(key) {
            if h.is_empty() {
                continue;
            }
            let p = PathBuf::from(h).join(".cargo");
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// Каталог архитектуры внутри webview2-com-sys (x64/arm64/x86).
fn arch_dir(arch: &str) -> &'static str {
    match arch {
        "aarch64" => "arm64",
        "x86" => "x86",
        _ => "x64",
    }
}

/// Поиск WebView2Loader.dll в реестре cargo по подпапке архитектуры.
fn find_loader(home: &Path, arch: &str) -> Option<PathBuf> {
    let src_root = home.join("registry").join("src");
    for idx in std::fs::read_dir(&src_root).into_iter().flatten().flatten() {
        for pkg in std::fs::read_dir(idx.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = pkg.file_name().to_string_lossy().into_owned();
            if !name.starts_with("webview2-com-sys-") {
                continue;
            }
            let dll = pkg.path().join(arch).join("WebView2Loader.dll");
            if dll.is_file() {
                return Some(dll);
            }
        }
    }
    None
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Только Windows+GNU: на Linux у x86_64-unknown-linux-gnu TARGET_ENV тоже
    // «gnu», поэтому привязка именно к ОС, иначе скрипт искал бы DLL в CI.
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }
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

    let arch = arch_dir(&std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default());
    let mut homes: Vec<String> = Vec::new();
    for home in cargo_homes() {
        homes.push(format!("{}", home.display()));
        if let Some(dll) = find_loader(&home, arch) {
            std::fs::copy(&dll, &dest).unwrap_or_else(|e| {
                panic!("не удалось скопировать {} в {}: {e}", dll.display(), dest.display())
            });
            println!(
                "cargo:warning=WebView2Loader.dll скопирован в {}",
                profile_dir.display()
            );
            return;
        }
    }
    panic!(
        "WebView2Loader.dll не найден в registry/src/*/webview2-com-sys-*/{arch}/ \
         (дома cargo: [{}]) — окно WebView2 не запустится без этой DLL. \
         Проверьте, что пакет webview2-com-sys распакован в cargo home раннера.",
        homes.join(", ")
    );
}
