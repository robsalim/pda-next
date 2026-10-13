//! Разбор и форматирование адресов Modbus.
//!
//! Четыре карты памяти, каждая со своим набором функционалов:
//!     HR — Holding Registers (4x)  FC03 чтение, FC06/16 запись
//!     IR — Input Registers   (3x)  FC04 чтение
//!     C  — Coils             (0x)  FC01 чтение, FC05/15 запись
//!     DI — Discrete Inputs   (1x)  FC02 чтение
//!
//! Смещения не смешиваются между областями: HR100 и IR100 — разные сигналы.

use once_cell::sync::Lazy;
use regex::Regex;

pub const AREA_HOLDING: &str = "HR";
pub const AREA_INPUT: &str = "IR";
pub const AREA_COIL: &str = "C";
pub const AREA_DISCRETE: &str = "DI";

/// Порядок отображения областей в интерфейсе: (код, подпись).
pub const MODBUS_AREAS: [(&str, &str); 4] = [
    (AREA_HOLDING, "HR — Holding Registers (читается/пишется)"),
    (AREA_INPUT, "IR — Input Registers (только чтение)"),
    (AREA_COIL, "C — Coils / дискретные выходы (читается/пишется)"),
    (AREA_DISCRETE, "DI — Discrete Inputs / дискретные входы (только чтение)"),
];

/// Сервер эмулирует только записываемые карты — IR/DI наполнять нечем.
pub const MODBUS_AREAS_SERVER: [(&str, &str); 2] = [
    (AREA_HOLDING, "HR — Holding Registers (читается/пишется)"),
    (AREA_COIL, "C — Coils / дискретные выходы (читается/пишется)"),
];

pub const MAX_REGS_PER_REQUEST: u32 = 125; // FC03/FC04
pub const MAX_BITS_PER_REQUEST: u32 = 2000; // FC01/02/05/15
pub const MAX_OFFSET: u32 = 0xFFFF;

/// Битовые области: значения — bool, а не 16-битные слова.
pub fn is_bit_area(area: &str) -> bool {
    matches!(area, AREA_COIL | AREA_DISCRETE)
}

/// Алиасы префиксов -> код области (порядок: длинные совпадения раньше).
static PREFIXES: [(&str, &str); 11] = [
    ("HOLDING", AREA_HOLDING),
    ("HOLD", AREA_HOLDING),
    ("DISCRETE", AREA_DISCRETE),
    ("DISC", AREA_DISCRETE),
    ("INPUT", AREA_INPUT),
    ("COIL", AREA_COIL),
    ("HR", AREA_HOLDING),
    ("IR", AREA_INPUT),
    ("DI", AREA_DISCRETE),
    ("H", AREA_HOLDING),
    ("C", AREA_COIL),
];

static ADDR_RE: Lazy<Regex> = Lazy::new(|| {
    let alts = PREFIXES.iter().map(|(p, _)| *p).collect::<Vec<_>>().join("|");
    Regex::new(&format!(r"^(?:(?P<prefix>{alts})\s*)?(?P<offset>\d{{1,6}})$").to_string())
        .unwrap()
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModbusAddress {
    pub area: String,
    pub offset: u32,
}

impl ModbusAddress {
    pub fn new(area: impl Into<String>, offset: u32) -> Self {
        Self { area: area.into(), offset }
    }

    pub fn is_bit(&self) -> bool {
        is_bit_area(&self.area)
    }

    /// Сколько ячеек области занимает сигнал (FLOAT = 2 регистра).
    pub fn size(&self, data_type: &str) -> u32 {
        if self.is_bit() {
            1
        } else if data_type.trim().eq_ignore_ascii_case("FLOAT") {
            2
        } else {
            1
        }
    }

    /// Каноническая строка: HR — обычным числом (обратная совместимость).
    pub fn to_string(&self) -> String {
        if self.area == AREA_HOLDING {
            self.offset.to_string()
        } else {
            format!("{}{}", self.area, self.offset)
        }
    }
}

/// Разбирает строку адреса; `None` — если не читается.
/// Число без префикса трактуется как Holding Register.
pub fn parse_modbus_address(address: &str, _data_type: &str) -> Option<ModbusAddress> {
    let mut text = address.trim().to_uppercase().replace(' ', "");
    text = text.trim_matches('%').to_string();
    if text.is_empty() {
        return None;
    }
    // Классическая запись области цифрами: 4x/3x/0x/1x
    let mut area: Option<&str> = None;
    for (pre, a) in [("4X", AREA_HOLDING), ("3X", AREA_INPUT), ("1X", AREA_DISCRETE), ("0X", AREA_COIL)] {
        if text.starts_with(pre) {
            area = Some(a);
            text = text[pre.len()..].to_string();
            break;
        }
    }
    let caps = ADDR_RE.captures(&text)?;
    if area.is_none() {
        let prefix = caps.name("prefix").map(|m| m.as_str()).unwrap_or("");
        area = Some(
            PREFIXES
                .iter()
                .find(|(p, _)| *p == prefix)
                .map(|(_, a)| *a)
                .unwrap_or(AREA_HOLDING),
        );
    }
    let offset: u32 = caps.name("offset")?.as_str().parse().ok()?;
    if offset > MAX_OFFSET {
        return None;
    }
    Some(ModbusAddress { area: area.unwrap().to_string(), offset })
}

/// Текст ошибки для адреса либо `None`, если всё корректно.
pub fn validate_modbus_address(address: &str, data_type: &str) -> Option<String> {
    let text = address.trim();
    if text.is_empty() {
        return Some("Адрес не заполнен".into());
    }
    let addr = match parse_modbus_address(text, data_type) {
        Some(a) => a,
        None => {
            return Some(format!(
                "Не понимаю адрес '{text}'. Нужен адрес Modbus: число (HR) либо \
                 с областью — HR100, IR200, C7, DI15"
            ))
        }
    };
    let dtype = data_type.trim().to_uppercase();
    if addr.is_bit() && !dtype.is_empty() && dtype != "BOOL" {
        return Some(format!(
            "Тип {dtype} не применяется к области {}: Coils и Discrete Inputs — \
             биты, для них нужен BOOL",
            addr.area
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_number_is_holding() {
        assert_eq!(parse_modbus_address("100", ""), Some(ModbusAddress::new(AREA_HOLDING, 100)));
        assert_eq!(parse_modbus_address("40001", ""), Some(ModbusAddress::new(AREA_HOLDING, 40001)));
    }

    #[test]
    fn prefixes_and_aliases() {
        assert_eq!(parse_modbus_address("HR100", ""), Some(ModbusAddress::new(AREA_HOLDING, 100)));
        assert_eq!(parse_modbus_address("hr 100", ""), Some(ModbusAddress::new(AREA_HOLDING, 100)));
        assert_eq!(parse_modbus_address("INPUT10", ""), Some(ModbusAddress::new(AREA_INPUT, 10)));
        assert_eq!(parse_modbus_address("COIL3", ""), Some(ModbusAddress::new(AREA_COIL, 3)));
        assert_eq!(parse_modbus_address("DISC2", ""), Some(ModbusAddress::new(AREA_DISCRETE, 2)));
        assert_eq!(parse_modbus_address("%C7", ""), Some(ModbusAddress::new(AREA_COIL, 7)));
    }

    #[test]
    fn numeric_area_forms() {
        assert_eq!(parse_modbus_address("4x100", ""), Some(ModbusAddress::new(AREA_HOLDING, 100)));
        assert_eq!(parse_modbus_address("3X7", ""), Some(ModbusAddress::new(AREA_INPUT, 7)));
        assert_eq!(parse_modbus_address("0X15", ""), Some(ModbusAddress::new(AREA_COIL, 15)));
        assert_eq!(parse_modbus_address("1X15", ""), Some(ModbusAddress::new(AREA_DISCRETE, 15)));
    }

    #[test]
    fn bad_addresses_rejected() {
        assert_eq!(parse_modbus_address("", ""), None);
        assert_eq!(parse_modbus_address("qwerty", ""), None);
        assert_eq!(parse_modbus_address("HR70000", ""), None); // > MAX_OFFSET
    }

    #[test]
    fn float_occupies_two_registers() {
        let a = ModbusAddress::new(AREA_HOLDING, 40001);
        assert_eq!(a.size("FLOAT"), 2);
        assert_eq!(a.size("INT16"), 1);
        assert_eq!(ModbusAddress::new(AREA_COIL, 7).size("BOOL"), 1);
    }

    #[test]
    fn canonical_strings() {
        assert_eq!(ModbusAddress::new(AREA_HOLDING, 100).to_string(), "100");
        assert_eq!(ModbusAddress::new(AREA_COIL, 7).to_string(), "C7");
    }

    #[test]
    fn validate_type_area_mismatch() {
        assert!(validate_modbus_address("C7", "BOOL").is_none());
        assert!(validate_modbus_address("C7", "INT16").is_some());
        assert!(validate_modbus_address("100", "BOOL").is_none());
        assert!(validate_modbus_address("", "FLOAT").is_some());
    }
}
