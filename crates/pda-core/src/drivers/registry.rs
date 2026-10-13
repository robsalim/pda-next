//! Реестр драйверов.
//!
//! Каноническое имя типа драйвера совпадает с названием библиотеки, на которой
//! драйвер построен: 'snap7', 'modbus_client', 'modbus_server'. Старые имена из
//! БД/JSON приводятся к каноническим через LEGACY_DRIVER_TYPES.

/// Ключ -> подпись для интерфейса.
pub fn get_driver_label(driver_type: &str) -> String {
    match normalize_driver_type(driver_type).as_str() {
        "snap7" => "snap7 (Siemens S7-300/400/1200/1500)".into(),
        "snap7_s200" => "snap7 (Siemens S7-200 SMART, V-память)".into(),
        "modbus_client" => "tokio-modbus (Modbus TCP Client)".into(),
        "modbus_server" => "Modbus TCP Server (встроенный)".into(),
        other => other.into(),
    }
}

/// Приводит любое написание типа драйвера к каноническому имени.
pub fn normalize_driver_type(value: &str) -> String {
    let key = value.trim().to_lowercase();
    match key.as_str() {
        "s7_1200" | "s7-1200" | "s7_300" | "s7_1500" | "siemens_s7" | "siemens" | "s7" => {
            "snap7".into()
        }
        "s7_200" | "s7-200" | "s7_200_smart" | "s7-200-smart" | "s7-200 smart"
        | "s7 200 smart" | "snap7_s200_smart" | "snap7-s200" => "snap7_s200".into(),
        _ => key,
    }
}

/// Список канонических имён типов драйверов в порядке отображения.
pub fn driver_type_keys() -> Vec<&'static str> {
    vec!["snap7", "snap7_s200", "modbus_client", "modbus_server"]
}

/// Известен ли такой тип драйвера.
pub fn is_known(driver_type: &str) -> bool {
    matches!(
        normalize_driver_type(driver_type).as_str(),
        "snap7" | "snap7_s200" | "modbus_client" | "modbus_server"
    )
}

/// Подпись адреса для конструктора в интерфейсе: (hint, области, примеры).
/// Пример: (адрес, тип данных, описание).
pub fn address_info(driver_type: &str) -> serde_json::Value {
    use super::{modbus_client, modbus_server, snap7};
    let key = normalize_driver_type(driver_type);
    match key.as_str() {
        "snap7" => snap7::address_info(false),
        "snap7_s200" => snap7::address_info(true),
        "modbus_client" => modbus_client::address_info(),
        "modbus_server" => modbus_server::address_info(),
        _ => serde_json::json!({ "hint": "", "areas": [], "examples": [] }),
    }
}

/// Возвращает текст ошибки для адреса либо None, если адрес корректен.
pub fn validate_address(driver_type: &str, address: &str, data_type: &str) -> Option<String> {
    use super::{modbus_client, modbus_server, snap7};
    let key = normalize_driver_type(driver_type);
    match key.as_str() {
        "snap7" => snap7::validate_address(address, data_type, false),
        "snap7_s200" => snap7::validate_address(address, data_type, true),
        "modbus_client" => modbus_client::validate_address(address, data_type),
        "modbus_server" => modbus_server::validate_address(address, data_type),
        _ => None,
    }
}
