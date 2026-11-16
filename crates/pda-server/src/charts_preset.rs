//! Наборы сигналов на вкладке Charts — хранение на стороне сервера.
//!
//! Хранилище общее для всех клиентов: файл лежит рядом с `db_config.json` и
//! доступен любому браузеру, открывшему интерфейс этого сервера. Не
//! localStorage: desktop-оболочка поднимает сервер на эфемерном порту
//! (`127.0.0.1:0`), а localStorage изолирован по origin (host:port), поэтому
//! браузерное хранилище не пережило бы перезапуск `pda-desktop.exe`.
//!
//! В хранилище только именованные наборы, сохранённые кнопкой
//! «💾 Сохранить набор». Текущий состав НЕ автозаписывается: старт и
//! обновление вкладки — пустой график, выбор одного клиента не меняет экран
//! другим. Файл `pda_charts_presets.json`:
//! ```json
//! {
//!   "named": { "Цех 1 — линия": { "windows": [[12]], "active": 0, "saved_at": "..." } }
//! }
//! ```
//! Ключ `last` (старое автосохранилище состава) больше не пишется и
//! игнорируется при чтении — файлы прошлых версий совместимы.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// Имя файла наборов (рядом с db_config.json).
pub const PRESET_FILE: &str = "pda_charts_presets.json";
/// Устаревший ключ автосохранения состава. Больше не пишется; удаляется из
/// файлов старых версий при первой записи набора.
const LAST_KEY: &str = "last";

/// Разумные пределы: состав приходит из UI, но файл правится и руками.
const MAX_WINDOWS: usize = 32;
const MAX_TAGS_TOTAL: usize = 1024;
const MAX_NAME_LEN: usize = 64;

/// Запись/чтение идут read-modify-write, поэтому их надо сериализовать:
/// два параллельных POST не должны потерять набор друг друга.
static STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Каталог наборов — тот же, где `db_config.json` и `pda_paused.json`.
pub fn presets_path(db_config_path: &Path) -> PathBuf {
    db_config_path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .join(PRESET_FILE)
}

/// Чтение файла. Нет файла или битый JSON — пустой набор (не ошибка: первый
/// запуск и ручная правка конфига не должны ломать вкладку).
fn load(path: &Path) -> Value {
    match std::fs::read_to_string(path) {
        Ok(txt) => match serde_json::from_str::<Value>(&txt) {
            Ok(v) if v.is_object() => v,
            _ => {
                tracing::warn!("{}: не разобран, начинаем с пустых наборов", path.display());
                json!({})
            }
        },
        Err(_) => json!({}),
    }
}

fn save(path: &Path, data: &Value) -> Result<(), String> {
    let txt = serde_json::to_string_pretty(data).map_err(|e| e.to_string())?;
    std::fs::write(path, txt).map_err(|e| format!("{}: {e}", path.display()))
}

/// Приводит состав из запроса к каноническому виду или возвращает ошибку.
/// Оставляются только положительные целые id тегов; пустые окна сохраняются
/// (окно без сигналов — тоже состояние вкладки).
pub fn sanitize(data: &Value) -> Result<Value, String> {
    let windows = data
        .get("windows")
        .and_then(Value::as_array)
        .ok_or("нет массива windows")?;
    if windows.len() > MAX_WINDOWS {
        return Err(format!("слишком много окон ({} > {MAX_WINDOWS})", windows.len()));
    }
    let mut total = 0usize;
    let mut clean: Vec<Value> = Vec::with_capacity(windows.len());
    for w in windows {
        let ids = match w {
            Value::Array(ids) => ids,
            Value::Null => &Vec::new(),
            _ => return Err("окно набора должно быть списком id сигналов".into()),
        };
        let mut row: Vec<i64> = Vec::with_capacity(ids.len());
        for id in ids {
            let id = id.as_i64().ok_or("id сигнала должен быть числом")?;
            if id <= 0 {
                continue;
            }
            total += 1;
            if total > MAX_TAGS_TOTAL {
                return Err(format!("слишком много сигналов (> {MAX_TAGS_TOTAL})"));
            }
            if !row.contains(&id) {
                row.push(id);
            }
        }
        clean.push(Value::Array(row.into_iter().map(Value::from).collect()));
    }
    let active = data
        .get("active")
        .and_then(Value::as_u64)
        .map(|a| (a as usize).min(clean.len().saturating_sub(1)))
        .unwrap_or(0);
    Ok(json!({
        "windows": clean,
        "active": active,
        "saved_at": chrono::Local::now().to_rfc3339(),
    }))
}

/// Имя набора: без управляющих символов и ограниченной длины. Пустое — ошибка
/// (пустое имя зарезервировано за автозаписью `last`).
fn clean_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("пустое имя набора".into());
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(format!("имя набора длиннее {MAX_NAME_LEN} символов"));
    }
    if name.chars().any(|c| c.is_control()) {
        return Err("имя набора содержит управляющие символы".into());
    }
    Ok(name.to_string())
}

/// Список для кнопки «📂 Загрузить набор»: только именованные наборы.
pub fn list(path: &Path) -> Value {
    let _g = STORE_LOCK.lock().ok();
    let store = load(path);
    let named = store.get("named").and_then(Value::as_object);
    let mut sets: Vec<Value> = named
        .map(|m| {
            m.iter()
                .map(|(name, v)| {
                    let windows = v.get("windows").and_then(Value::as_array);
                    let signals: usize = windows
                        .map(|ws| {
                            ws.iter()
                                .filter_map(Value::as_array)
                                .map(|w| w.len())
                                .sum()
                        })
                        .unwrap_or(0);
                    json!({
                        "name": name,
                        "windows": v.get("windows").cloned().unwrap_or(json!([])),
                        "active": v.get("active").cloned().unwrap_or(json!(0)),
                        "saved_at": v.get("saved_at").cloned().unwrap_or(Value::Null),
                        "signals": signals,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    // Порядок стабильный по имени — список не «прыгает» между открытиями.
    sets.sort_by(|a, b| {
        a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or(""))
    });
    json!({ "sets": sets })
}

/// Сохранить именованный набор (перезапись по имени — ожидаемое поведение
/// кнопки «Сохранить»). Пустого имени больше нет: это было автосоздание
/// общего «last» состава, из-за которого клиенты перезаписывали экран друг
/// друга.
pub fn put(path: &Path, name: &str, data: &Value) -> Result<Value, String> {
    let clean = sanitize(data)?;
    let name = clean_name(name)?;
    let _g = STORE_LOCK.lock().ok();
    let mut store = load(path);
    store["named"][&name] = clean.clone();
    // Старое автосохранилище вычищаем при первой записи — оно больше не нужно.
    if let Some(obj) = store.as_object_mut() {
        obj.remove(LAST_KEY);
    }
    save(path, &store)?;
    Ok(json!({
        "ok": true,
        "name": name,
        "saved_at": clean["saved_at"],
    }))
}

/// Удалить именованный набор. false — набора не было.
pub fn remove(path: &Path, name: &str) -> Result<bool, String> {
    let name = clean_name(name)?;
    let _g = STORE_LOCK.lock().ok();
    let mut store = load(path);
    let removed = store
        .get_mut("named")
        .and_then(Value::as_object_mut)
        .and_then(|m| m.remove(&name))
        .is_some();
    if removed {
        save(path, &store)?;
    }
    Ok(removed)
}

// -------------------------------------------------------------------- tests
#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_file(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pda_presets_{}_{}", tag, std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir.join(PRESET_FILE)
    }

    #[test]
    fn sanitize_keeps_positive_unique_ids() {
        let v = sanitize(&json!({"windows": [[5, 5, 0, -3, 7], []], "active": 9})).unwrap();
        assert_eq!(v["windows"], json!([[5, 7], []]), "нули/дубли вне набора, пустое окно живо");
        assert_eq!(v["active"], 1, "активное окно не за пределами списка");
        assert!(sanitize(&json!({"windows": "нет"})).is_err());
    }

    #[test]
    fn only_named_sets_are_stored() {
        let path = tmp_file("mix");
        let _ = std::fs::remove_file(&path);
        assert!(list(&path)["sets"].as_array().unwrap().is_empty(), "первый запуск пуст");
        assert!(list(&path).get("last").is_none(), "автосостава больше нет в ответе");

        // Пустое имя (бывшее автосохранение last) теперь запрещено.
        assert!(put(&path, "", &json!({"windows": [[1]], "active": 0})).is_err());

        put(&path, "Цех 1", &json!({"windows": [[2, 3]], "active": 0})).unwrap();
        put(&path, "Цех 2", &json!({"windows": [[4]], "active": 0})).unwrap();

        let l = list(&path);
        let names: Vec<&str> = l["sets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["Цех 1", "Цех 2"]);
        assert_eq!(l["sets"][0]["signals"], 2, "в списке видно размер набора");

        // Перезапись по имени не создаёт дубль.
        put(&path, "Цех 1", &json!({"windows": [[9]], "active": 0})).unwrap();
        assert_eq!(list(&path)["sets"].as_array().unwrap().len(), 2);
        assert_eq!(list(&path)["sets"][0]["windows"], json!([[9]]));

        assert!(remove(&path, "Цех 1").unwrap());
        assert!(!remove(&path, "Цех 1").unwrap(), "повторное удаление — просто false");
        assert_eq!(list(&path)["sets"].as_array().unwrap().len(), 1);

        // Файл от старой версии с ключом last: при записи он вычищается,
        // а именованные наборы прочитались бы и до записи.
        std::fs::write(
            &path,
            r#"{"last":{"windows":[[7]]},"named":{"Старый":{"windows":[[8]],"active":0}}}"#,
        )
        .unwrap();
        assert_eq!(list(&path)["sets"].as_array().unwrap().len(), 1, "старый last не мешает");
        put(&path, "Новый", &json!({"windows": [[1]], "active": 0})).unwrap();
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(raw.get("last").is_none(), "устаревший last удалён из файла");
        assert_eq!(raw["named"].as_object().unwrap().len(), 2);

        // Пустое имя зарезервировано, управляющие символы — ошибка.
        assert!(clean_name("   ").is_err());
        assert!(clean_name("a\u{1}b").is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
