//! Драйвер Siemens S7 поверх собственного транспорта `s7_proto`.
//!
//! S7-200 SMART отличается только разбором адреса: V-память (VB/VW/VD/V100.3)
//! сворачивается в DB1 и читается штатным db_read. Транспорт, цикл опроса,
//! пакетное чтение и метрики — общие.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Local;
use once_cell::sync::Lazy;
use regex::Regex;

use crate::core::tag_registry::TagRegistry;
use crate::models::{cfg_i64, cfg_str, Connection};

use super::base::{
    fmt_poll_status, poll_period, submit_values, update_stats, DriverState, TagCache,
};
use super::s7_proto::{
    AREA_DB, AREA_MK, AREA_PA, AREA_PE, MAX_VARS, S7Conn, S7Error, S7Item,
};

// ---------------------------------------------------------------------- адрес
/// Области памяти по букве адреса.
fn area_by_letter(letter: &str) -> Option<&'static str> {
    match letter {
        "M" => Some("MK"), // Memory (Merker)
        "I" | "E" => Some("PE"), // образ процесса входа
        "Q" | "A" | "P" => Some("PA"), // образ процесса выхода
        _ => None,
    }
}

fn area_code(area: &str) -> u8 {
    match area {
        "DBD" | "DBW" | "DBB" | "DBX" => AREA_DB,
        "MK" => AREA_MK,
        "PE" => AREA_PE,
        "PA" => AREA_PA,
        _ => AREA_DB,
    }
}

/// Буква размера: W=2, B=1, D=4, X=1 (бит).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeLetter {
    W,
    B,
    D,
    X,
}

impl SizeLetter {
    pub fn byte_len(self) -> u16 {
        match self {
            SizeLetter::W => 2,
            SizeLetter::B => 1,
            SizeLetter::D => 4,
            SizeLetter::X => 1,
        }
    }
    fn letter(self) -> &'static str {
        match self {
            SizeLetter::W => "W",
            SizeLetter::B => "B",
            SizeLetter::D => "D",
            SizeLetter::X => "X",
        }
    }
    fn from_letter(c: &str) -> Option<Self> {
        Some(match c {
            "W" => SizeLetter::W,
            "B" => SizeLetter::B,
            "D" => SizeLetter::D,
            "X" => SizeLetter::X,
            _ => return None,
        })
    }
}

/// Размер по умолчанию, если буква размера в адресе не указана.
fn size_by_data_type(data_type: &str) -> SizeLetter {
    match data_type.trim().to_uppercase().as_str() {
        "FLOAT" | "DWORD" => SizeLetter::D,
        "INT16" | "UINT16" | "INT" | "WORD" => SizeLetter::W,
        "BOOL" => SizeLetter::X,
        _ => SizeLetter::B,
    }
}

#[derive(Debug, Clone)]
pub struct S7Address {
    /// "DB" — блок данных, "AREA" — M / I / Q. V-память — kind="DB" (db=1).
    pub kind: &'static str,
    /// Для DB: DBD/DBW/DBB/DBX; для AREA: MK/PE/PA.
    pub area: String,
    pub db: u16,
    pub size: SizeLetter,
    pub offset: u32,
    pub bit: u8,
    /// V-память S7-200 SMART: читается как DB1, в ошибках показывается VB/VW/VD.
    pub v_area: bool,
}

impl S7Address {
    pub fn byte_len(&self) -> u16 {
        self.size.byte_len()
    }
}

/// '%MW230' (стиль TIA Portal) и 'mw 230' -> 'MW230'.
pub fn normalize_address(address: &str) -> String {
    address.trim().to_uppercase().replace('%', "").replace(' ', "").replace('\t', "")
}

static RE_DB: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^DB(\d+)\.(DBD|DBW|DBB|DBX)(\d+)(?:\.(\d+))?$").unwrap());
static RE_MNEMONIC: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^([MIEQAP])([WBXD]?)(\d+)(?:\.(\d+))?$").unwrap());
static RE_V: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^V([WBXD]?)(\d+)(?:\.(\d+))?$").unwrap());

fn parse_db_token(addr: &str) -> Option<S7Address> {
    let c = RE_DB.captures(addr)?;
    let token = c.get(2).unwrap().as_str();
    Some(S7Address {
        kind: "DB",
        area: token.to_string(),
        db: c[1].parse().unwrap_or(0) as u16,
        size: SizeLetter::from_letter(&token[2..])?,
        offset: c[3].parse().unwrap_or(0),
        bit: c.get(4).map(|m| m.as_str().parse().unwrap_or(0)).unwrap_or(0) as u8,
        v_area: false,
    })
}

fn parse_mnemonic(addr: &str, data_type: &str) -> Option<S7Address> {
    let c = RE_MNEMONIC.captures(addr)?;
    let area = area_by_letter(&c[1])?;
    let bit: Option<u32> = c.get(4).map(|m| m.as_str().parse().unwrap_or(0));
    let size = match bit {
        Some(_) => SizeLetter::X,
        None => match SizeLetter::from_letter(&c[2]) {
            Some(s) => s,
            None => size_by_data_type(data_type),
        },
    };
    Some(S7Address {
        kind: "AREA",
        area: area.to_string(),
        db: 0,
        size,
        offset: c[3].parse().unwrap_or(0),
        bit: bit.unwrap_or(0) as u8,
        v_area: false,
    })
}

/// Адреса старших S7: DB1.DBW4, MW230, M3.4, I0.1, QW0 ...
pub fn parse_s7_address(address: &str, data_type: &str) -> Option<S7Address> {
    let addr = normalize_address(address);
    if addr.is_empty() {
        return None;
    }
    if let Some(a) = parse_db_token(&addr) {
        return Some(a);
    }
    parse_mnemonic(&addr, data_type)
}

/// Разбор V-памяти S7-200 SMART: VB/VW/VD или бит V100.3 (без буквы X).
fn parse_v_token(addr: &str, data_type: &str) -> Option<S7Address> {
    let c = RE_V.captures(addr)?;
    let letter = &c[1];
    let bit: Option<u32> = c.get(3).map(|m| m.as_str().parse().unwrap_or(0));
    let size = if bit.is_some() {
        SizeLetter::X
    } else if letter.is_empty() || letter == "X" {
        // без буквы размер берём из типа; X явно означает бит
        if letter == "X" {
            SizeLetter::X
        } else {
            size_by_data_type(data_type)
        }
    } else {
        SizeLetter::from_letter(letter)?
    };
    let token = format!("DB{}", size.letter()); // DBB / DBW / DBD / DBX
    Some(S7Address {
        kind: "DB",
        area: token,
        db: 1, // V-память S7-200 SMART = DB1
        size,
        offset: c[2].parse().unwrap_or(0),
        bit: bit.unwrap_or(0) as u8,
        v_area: true,
    })
}

/// Адреса S7-200 SMART: V-память + штатные I/Q/M + старые DB-записи.
pub fn parse_s200_address(address: &str, data_type: &str) -> Option<S7Address> {
    let addr = normalize_address(address);
    if addr.is_empty() {
        return None;
    }
    if let Some(a) = parse_v_token(&addr, data_type) {
        return Some(a);
    }
    if let Some(a) = parse_mnemonic(&addr, data_type) {
        return Some(a);
    }
    // DB*.DBx: у SMART нет блоков данных, но «DB1.DBW100» — тот же байт V100.
    // Принимаем, чтобы старые теги не ломались при смене типа.
    parse_db_token(&addr)
}

// ------------------------------------------------------------- метаданные GUI
const HINT_CLASSIC: &str = "Примеры: MW230, MD100, M3.4, I0.1, Q0.0, DB1.DBW4, DB1.DBD0, DB1.DBX0.0";
const HINT_S200: &str = "Примеры: VW100, VB5, VD20, V100.3, IW0, QW0, MW10";

const FORMATS_CLASSIC: &str = "MW230, MB5, MD100, MX3.4, IW64, QW0, DB1.DBW4, DB5.DBD0, DB1.DBX0.0";
const FORMATS_S200: &str =
    "VB100, VW100, VD100, V100.3 (V-память), IW0/IX0.0, QW0/QX0.1, MW10/MX3.4";

/// Подпись адреса для конструктора в интерфейсе (hint, области, примеры).
pub fn address_info(is_s200: bool) -> serde_json::Value {
    if is_s200 {
        serde_json::json!({
            "hint": HINT_S200,
            "areas": [
                ["V", "V — V-память (DB1)"],
                ["M", "M — Merker (флаги)"],
                ["I", "I — образ входа"],
                ["Q", "Q — образ выхода"],
            ],
            "examples": [
                ["VW100", "INT16", "V-память, слово — 2 байта VW100 (читается из DB1)"],
                ["VB5", "BYTE", "V-память, байт VB5"],
                ["VD20", "FLOAT", "V-память, двойное слово VD20 (REAL)"],
                ["VD20", "DWORD", "V-память, беззнаковое 32-битное VD20"],
                ["V100.3", "BOOL", "V-память, бит 3 в байте 100 (без буквы X)"],
                ["IW0", "INT16", "Образ входа (I), 2 байта с IW0"],
                ["QW0", "INT16", "Образ выхода (Q), 2 байта с QW0"],
                ["M10.0", "BOOL", "Merker, бит 0 в байте MB10"],
                ["MW10", "INT16", "Merker word — 2 байта MB10"],
            ],
        })
    } else {
        serde_json::json!({
            "hint": HINT_CLASSIC,
            "areas": [
                ["DB", "DB — блок данных"],
                ["M", "M — Merker (флаги)"],
                ["I", "I — образ входа"],
                ["Q", "Q — образ выхода"],
            ],
            "examples": [
                ["MW230", "INT16", "Merker word — 2 байта, начиная с MB230"],
                ["MB5", "BYTE", "Merker byte — 1 байт MB5"],
                ["MD100", "FLOAT", "Merker dword — 4 байта MD100 (REAL)"],
                ["M3.4", "BOOL", "Бит 4 в байте MB3 (без буквы X)"],
                ["I0.1", "BOOL", "Бит 1 в байте IB0 (без буквы X)"],
                ["Q0.0", "BOOL", "Бит 0 в байте QB0 (без буквы X)"],
                ["IW64", "INT16", "Образ входа (I), 2 байта с IW64"],
                ["QW0", "INT16", "Образ выхода (Q), 2 байта с QW0"],
                ["DB1.DBW4", "INT16", "Слово в блоке данных DB1, смещение 4"],
                ["DB5.DBD0", "FLOAT", "Real в DB5, смещение 0"],
                ["DB5.DBD0", "DWORD", "Беззнаковое 32-битное в DB5, смещение 0"],
                ["DB1.DBX0.0", "BOOL", "Бит 0 в байте 0 блока DB1"],
            ],
        })
    }
}

/// Возвращает текст ошибки для адреса либо None.
pub fn validate_address(address: &str, data_type: &str, is_s200: bool) -> Option<String> {
    if address.trim().is_empty() {
        return Some("Адрес не заполнен".into());
    }
    let ok = if is_s200 {
        parse_s200_address(address, data_type).is_some()
    } else {
        parse_s7_address(address, data_type).is_some()
    };
    if !ok {
        let hint = if is_s200 { FORMATS_S200 } else { FORMATS_CLASSIC };
        return Some(format!("Не понимаю адрес '{address}'. Форматы: {hint}"));
    }
    None
}

// ---------------------------------------------------------------- извлечение
/// Достаёт значение тега из буфера пакета.
/// Порядок байтов — большой (big-endian), как в snap7.util:
/// get_int/get_uint '>h'/'>H', get_real '>f', get_dword '>I', get_byte 'B'.
fn extract(buf: &[u8], rel: usize, a: &S7Address, data_type: &str) -> Option<f64> {
    let need = a.byte_len() as usize;
    if rel + need > buf.len() {
        return None;
    }
    match a.size {
        SizeLetter::X => {
            let byte = buf[rel] as u32;
            let mask: u32 = 1u32.checked_shl(a.bit as u32).unwrap_or(0);
            Some(if byte & mask == mask { 1.0 } else { 0.0 })
        }
        SizeLetter::D => {
            let word = u32::from_be_bytes([buf[rel], buf[rel + 1], buf[rel + 2], buf[rel + 3]]);
            Some(if data_type.trim().eq_ignore_ascii_case("FLOAT") {
                f32::from_bits(word) as f64
            } else {
                word as f64
            })
        }
        SizeLetter::W => {
            let w = u16::from_be_bytes([buf[rel], buf[rel + 1]]);
            Some(if data_type.trim().eq_ignore_ascii_case("UINT16") {
                w as f64
            } else {
                w as i16 as f64
            })
        }
        SizeLetter::B => Some(buf[rel] as f64),
    }
}

// --------------------------------------------------------------------- драйвер
/// Точка входа: цикл опроса одного подключения snap7 / snap7_s200.
/// Запускается как tokio-задача; остановка — снятием флага state.running
/// (менеджер при необходимости прерывает задачу снаружи).
pub async fn run_snap7(conn_cfg: Arc<Connection>, registry: Arc<TagRegistry>, state: Arc<DriverState>) {
    let is_s200 = crate::drivers::registry::normalize_driver_type(&conn_cfg.driver_type) == "snap7_s200";
    let ip = cfg_str(&conn_cfg.config, "ip", "192.168.0.1");
    let port = cfg_i64(&conn_cfg.config, "port", 102).clamp(1, 65535) as u16;
    let rack = cfg_i64(&conn_cfg.config, "rack", 0).clamp(0, 7) as u8;
    let slot = cfg_i64(&conn_cfg.config, "slot", 1).clamp(0, 31) as u8;
    // TSAP: 1 (PG) для старших S7; для SMART — 3 (Basic) => удалённый 0x0301
    let tsap_type = cfg_i64(&conn_cfg.config, "connection_type", if is_s200 { 3 } else { 1 })
        .clamp(1, 7) as u8;

    let addr = match ip.parse::<IpAddr>() {
        Ok(a) => SocketAddr::new(a, port),
        Err(e) => {
            state.set_status(format!("Connect Error: неверный IP '{ip}' ({e})"));
            state.running.store(false, std::sync::atomic::Ordering::SeqCst);
            return;
        }
    };

    let period = poll_period(&conn_cfg);
    let mut cache = TagCache::new();
    let mut client: Option<S7Conn> = None;
    // Максимум байт в одном запросе; уточняется после подключения
    let mut max_read_len: u16 = 200;
    let merge_gap: u32 = 16; // разрыв, выгоднее прочитать «заодно»

    state.running.store(true, std::sync::atomic::Ordering::SeqCst);
    while state.running.load(std::sync::atomic::Ordering::Relaxed) {
        // ------------------------------------------------ reconnect
        if client.is_none() {
            state.set_status("Connecting...");
            match S7Conn::connect(addr, rack, slot, tsap_type, Duration::from_millis(3000)).await {
                Ok(mut c) => {
                    c.set_timeout(period.max(Duration::from_millis(2000)).min(Duration::from_millis(5000)));
                    // Ограничиваем длину пакета согласованным PDU
                    let pdu = c.pdu_size;
                    max_read_len = (pdu.saturating_sub(32)).clamp(48, 222);
                    client = Some(c);
                    cache.invalidate(); // теги/адреса могли измениться
                    state.set_status("Connected");
                }
                Err(e) => {
                    let text = e.text();
                    *state.last_error.lock() = Some(text.clone());
                    state.set_status(format!("Connect Error: {text}"));
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            }
        }

        // ------------------------------------------------ цикл опроса
        let cycle_start = Instant::now();
        let mut last_error: Option<String> = None;
        let mut transport_fault = false;

        let t0 = Instant::now();
        let tags = cache.get(&registry, conn_cfg.id);
        let db_ms = t0.elapsed().as_secs_f64();

        let mut values: HashMap<i64, f64> = HashMap::new();
        let mut c = client.take().expect("есть соединение");
        let mut req_count = 0usize;
        match read_all_values(&mut c, &tags, is_s200, max_read_len, merge_gap, &mut req_count).await {
            Ok(Ok(v)) => values = v,
            Ok(Err(e)) => {
                // ошибки пакетного чтения остаются «мягкими», но fault-код — переподключение
                last_error = Some(e.text());
                transport_fault = is_transport_fault(&e);
            }
            Err(e) => {
                last_error = Some(e.text());
                transport_fault = is_transport_fault(&e);
            }
        }
        let requests = req_count;
        let read_ms = t0.elapsed().as_secs_f64() - db_ms;

        if transport_fault {
            drop(c); // соединение «протухло» — в следующем цикле переподключимся
        } else {
            client = Some(c);
        }

        if last_error.is_none() {
            let now = Local::now();
            submit_values(&registry, &values, now);
            // Теги, не вошедшие в values, — плохое качество
            let missing: Vec<i64> = tags
                .iter()
                .filter(|t| !values.contains_key(&t.tag_id))
                .map(|t| t.tag_id)
                .collect();
            super::base::mark_cycle_errors(&registry, missing);
        }

        let cycle_s = cycle_start.elapsed().as_secs_f64();
        update_stats(&state, cycle_s, read_ms, db_ms, requests, tags.len());

        if let Some(err) = last_error {
            *state.last_error.lock() = Some(err.clone());
            state.set_status(format!("Polling (err): {err}"));
        } else {
            *state.last_error.lock() = None;
            state.set_status(fmt_poll_status(&state, period, cycle_s));
        }

        // poll_interval_ms = целевой ПЕРИОД, поэтому спим только остаток цикла
        let elapsed = cycle_start.elapsed();
        if elapsed < period {
            tokio::time::sleep(period - elapsed).await;
        }
    }

    state.set_status("Stopped");
}

fn is_transport_fault(e: &S7Error) -> bool {
    matches!(e, S7Error::Io(_) | S7Error::Timeout)
}

/// Читает все теги за минимальное число round-trip.
/// Приоритет — S7 multi-read (до MAX_VARS адресов в PDU);
/// при сбое — пакетные чтения по объединённым областям.
async fn read_all_values(
    c: &mut S7Conn,
    tags: &[Arc<crate::core::tag_point::TagPoint>],
    is_s200: bool,
    max_read_len: u16,
    merge_gap: u32,
    req_count: &mut usize,
) -> Result<Result<HashMap<i64, f64>, S7Error>, S7Error> {
    // Собираем адреса один раз.
    let mut addrs: Vec<(i64, String, S7Address)> = Vec::with_capacity(tags.len());
    for t in tags {
        let a = parse_any(t.address_str().as_str(), t.data_type().as_str(), is_s200);
        if let Some(a) = a {
            addrs.push((t.tag_id, t.data_type(), a));
        }
    }
    if addrs.is_empty() {
        return Ok(Ok(HashMap::new()));
    }

    // ---- multi-read: сколько элементов влезает в согласованный PDU
    let pdu = c.pdu_size as usize;
    let per_req = pdu.saturating_sub(48) / 12; // param 10 + resp 8 на элемент
    let cap = per_req.min(MAX_VARS).max(1);
    let mut values: HashMap<i64, f64> = HashMap::new();
    for chunk in addrs.chunks(cap) {
        let items: Vec<S7Item> = chunk
            .iter()
            .map(|(_, _, a)| S7Item {
                area: area_code(a.area.as_str()),
                db: if a.kind == "DB" { a.db } else { 0 },
                start: a.offset,
                len: a.byte_len(),
            })
            .collect();
        match c.read_multi(&items).await {
            Ok(res) => {
                *req_count += 1;
                for ((tid, dtype, a), buf) in chunk.iter().zip(res.iter()) {
                    if let Ok(b) = buf {
                        if let Some(v) = extract(b, 0, a, dtype) {
                            values.insert(*tid, v);
                        }
                    }
                }
            }
            Err(e) if is_transport_fault(&e) => return Err(e),
            Err(_e) => {
                // «мягкая» ошибка multi-read (например, special-область) —
                // читаем всёMissing дочитываются пакетами через план
                let plan = plan_reads(&addrs, max_read_len as u32, merge_gap);
                let v = read_plan(c, &plan, req_count).await;
                return Ok(v);
            }
        }
    }
    Ok(Ok(values))
}

fn parse_any(address: &str, data_type: &str, is_s200: bool) -> Option<S7Address> {
    if is_s200 {
        parse_s200_address(address, data_type)
    } else {
        parse_s7_address(address, data_type)
    }
}

/// Один блок плана пакетного чтения: (kind, area, db, start, length, участники).
pub struct ReadBlock {
    pub kind: &'static str,
    pub area: String,
    pub db: u16,
    pub start: u32,
    pub length: u32,
    /// (tag_id, data_type, адрес, смещение в буфере пакета)
    pub members: Vec<(i64, String, S7Address, u32)>,
}

/// Группирует теги по области и объединяет соседние адреса в пакеты
/// (merge_gap=16, max_read_len из PDU).
fn plan_reads(addrs: &[(i64, String, S7Address)], max_read_len: u32, merge_gap: u32) -> Vec<ReadBlock> {
    // Все теги одного DB группируются вместе (db_read читает любые размеры);
    // области M/I/Q — по (area). kind у S7Address — &'static, поэтому
    // ключ живёт столько же, сколько план.
    let mut order: Vec<(&'static str, String, u16)> = Vec::new();
    let mut groups: HashMap<(&'static str, String, u16), Vec<(u32, u16, i64, String, S7Address)>> =
        HashMap::new();
    for (tid, dtype, a) in addrs {
        let key = (a.kind, if a.kind == "DB" { String::new() } else { a.area.clone() }, a.db);
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups
            .entry(key)
            .or_default()
            .push((a.offset, a.byte_len(), *tid, dtype.clone(), a.clone()));
    }

    let mut plan: Vec<ReadBlock> = Vec::new();
    for key in order {
        let items = groups.get_mut(&key).unwrap();
        items.sort_by_key(|(offset, _, _, _, _)| *offset);
        let (kind, area, db) = (key.0, key.1.clone(), key.2);
        let mut block: Option<(u32, u32, Vec<(i64, String, S7Address, u32)>)> = None;
        for (offset, length, tid, dtype, a) in items.iter() {
            let offset = *offset;
            let end = offset + (*length as u32);
            match &mut block {
                Some(b) if offset <= b.1 + merge_gap && (end - b.0) <= max_read_len => {
                    b.1 = b.1.max(end);
                    b.2.push((*tid, dtype.clone(), a.clone(), offset - b.0));
                }
                _ => {
                    if let Some(b) = block.take() {
                        plan.push(ReadBlock {
                            kind,
                            area: area.clone(),
                            db,
                            start: b.0,
                            length: b.1 - b.0,
                            members: b.2,
                        });
                    }
                    block = Some((offset, end, vec![(*tid, dtype.clone(), a.clone(), 0)]));
                }
            }
        }
        if let Some(b) = block.take() {
            plan.push(ReadBlock { kind, area, db, start: b.0, length: b.1 - b.0, members: b.2 });
        }
    }
    plan
}

/// Выполняет план пакетного чтения; ошибки блоков — мягкие, транспорт — фатальный.
async fn read_plan(
    c: &mut S7Conn,
    plan: &[ReadBlock],
    req_count: &mut usize,
) -> Result<HashMap<i64, f64>, S7Error> {
    let mut values: HashMap<i64, f64> = HashMap::new();
    let mut soft_err: Option<S7Error> = None;
    for blk in plan {
        let read = if blk.kind == "DB" {
            c.db_read(blk.db, blk.start, blk.length as u16).await
        } else {
            c.read_area(area_code(&blk.area), blk.start, blk.length as u16).await
        };
        let buf = match read {
            Ok(b) => {
                *req_count += 1;
                b
            }
            Err(e) if is_transport_fault(&e) => return Err(e),
            Err(e) => {
                // Пакет не читается целиком — пробуем каждый тег отдельно
                soft_err = Some(e);
                for (tid, dtype, a, _) in &blk.members {
                    let one = if blk.kind == "DB" {
                        c.db_read(a.db, a.offset, a.byte_len()).await
                    } else {
                        c.read_area(area_code(&a.area), a.offset, a.byte_len()).await
                    };
                    match one {
                        Ok(b) => {
                            *req_count += 1;
                            if let Some(v) = extract(&b, 0, a, dtype) {
                                values.insert(*tid, v);
                            }
                        }
                        Err(e) if is_transport_fault(&e) => return Err(e),
                        Err(e) => {
                            soft_err = Some(e);
                        }
                    }
                }
                continue;
            }
        };
        for (tid, dtype, a, rel) in &blk.members {
            if let Some(v) = extract(&buf, *rel as usize, a, dtype) {
                values.insert(*tid, v);
            }
        }
    }
    match soft_err {
        Some(e) => Err(e),
        None => Ok(values),
    }
}

// ---------------------------------------------------------------------- tests
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_addresses() {
        let a = parse_s7_address("DB1.DBW4", "INT16").unwrap();
        assert_eq!(a.kind, "DB");
        assert_eq!(a.db, 1);
        assert_eq!(a.size, SizeLetter::W);
        assert_eq!(a.offset, 4);

        let a = parse_s7_address("%DB5.DBD0", "FLOAT").unwrap();
        assert_eq!(a.db, 5);
        assert_eq!(a.size, SizeLetter::D);

        let a = parse_s7_address("db1.dbx0.0", "BOOL").unwrap();
        assert_eq!(a.size, SizeLetter::X);
        assert_eq!(a.bit, 0);
    }

    #[test]
    fn mnemonic_addresses() {
        let a = parse_s7_address("MW230", "INT16").unwrap();
        assert_eq!(a.kind, "AREA");
        assert_eq!(a.area, "MK");
        assert_eq!(a.size, SizeLetter::W);
        assert_eq!(a.offset, 230);

        // бит без буквы X — признак ".номер"
        let a = parse_s7_address("M3.4", "BOOL").unwrap();
        assert_eq!(a.size, SizeLetter::X);
        assert_eq!(a.bit, 4);

        let a = parse_s7_address("i0.1", "BOOL").unwrap();
        assert_eq!(a.area, "PE");

        let a = parse_s7_address("Q0.0", "BOOL").unwrap();
        assert_eq!(a.area, "PA");

        // размер без буквы берётся из типа
        let a = parse_s7_address("MD100", "FLOAT").unwrap();
        assert_eq!(a.size, SizeLetter::D);
        let a = parse_s7_address("MB5", "BYTE").unwrap();
        assert_eq!(a.size, SizeLetter::B);
    }

    #[test]
    fn s200_v_memory_maps_to_db1() {
        let a = parse_s200_address("VW100", "INT16").unwrap();
        assert_eq!(a.kind, "DB");
        assert!(a.v_area);
        assert_eq!(a.db, 1);
        assert_eq!(a.size, SizeLetter::W);
        assert_eq!(a.offset, 100);

        let a = parse_s200_address("V100.3", "BOOL").unwrap();
        assert_eq!(a.size, SizeLetter::X);
        assert_eq!(a.bit, 3);
        assert_eq!(a.db, 1);

        let a = parse_s200_address("VD20", "DWORD").unwrap();
        assert_eq!(a.size, SizeLetter::D);
        assert_eq!(a.area, "DBD");

        // I/Q/M работают как обычно
        let a = parse_s200_address("IW0", "INT16").unwrap();
        assert_eq!(a.area, "PE");

        // старый DB-адрес остаётся валидным (то же V100)
        let a = parse_s200_address("DB1.DBW100", "INT16").unwrap();
        assert_eq!(a.offset, 100);
        assert!(!a.v_area);

        // SMART не понимает DB без V? понимает DBx.DB — для совместимости
        assert!(parse_s200_address("XX10", "INT16").is_none());
    }

    #[test]
    fn validate_messages() {
        assert!(validate_address("", "FLOAT", false).is_some());
        assert!(validate_address("qwerty", "FLOAT", false).is_some());
        assert!(validate_address("MW10", "INT16", false).is_none());
        assert!(validate_address("VW100", "INT16", true).is_none());
        assert!(validate_address("MW10", "INT16", true).is_none());
    }

    #[test]
    fn extract_values() {
        // INT16 big-endian: [0x00, 0xFF] = 255
        let buf = [0x00u8, 0xFF, 0x42, 0xF6, 0xA4, 0x5A];
        let a = S7Address {
            kind: "DB", area: "DBW".into(), db: 1, size: SizeLetter::W, offset: 0, bit: 0, v_area: false,
        };
        assert_eq!(extract(&buf, 0, &a, "INT16").unwrap(), 255.0);
        // BOOL: бит 1 байта 0xFF (вторые два байта буфера — не наш случай)
        let a = S7Address { size: SizeLetter::X, bit: 1, ..a };
        assert_eq!(extract(&buf[1..], 0, &a, "BOOL").unwrap(), 1.0); // 0xFF
        assert_eq!(extract(&buf, 0, &a, "BOOL").unwrap(), 0.0); // 0x00
        // FLOAT big-endian: 0x42F6A45A = 123.321
        let a = S7Address {
            kind: "DB", area: "DBD".into(), db: 1, size: SizeLetter::D, offset: 2, bit: 0, v_area: false,
        };
        assert!((extract(&buf, 2, &a, "FLOAT").unwrap() - 123.32099914550781).abs() < 1e-3);

        // выход за границу буфера — None
        let a = S7Address { size: SizeLetter::D, offset: 0, ..a };
        assert!(extract(&buf[0..3], 0, &a, "FLOAT").is_none());
    }

    #[test]
    fn plan_merges_neighbors() {
        let addrs = vec![
            (1i64, "INT16".to_string(), parse_s7_address("DB1.DBW0", "INT16").unwrap()),
            (2, "INT16".to_string(), parse_s7_address("DB1.DBW4", "INT16").unwrap()),
            (3, "INT16".to_string(), parse_s7_address("DB1.DBW100", "INT16").unwrap()),
        ];
        let plan = plan_reads(&addrs, 200, 16);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].start, 0);
        assert_eq!(plan[0].length, 6); // W0 + W4 через зазор 2 <= 16
        assert_eq!(plan[0].members.len(), 2);
        assert_eq!(plan[1].start, 100);
        assert_eq!(plan[1].members[0].3, 0);
    }

    #[test]
    fn plan_respects_max_len() {
        let addrs = vec![
            (1i64, "INT16".to_string(), parse_s7_address("DB1.DBW0", "INT16").unwrap()),
            (2, "INT16".to_string(), parse_s7_address("DB1.DBW220", "INT16").unwrap()),
        ];
        let plan = plan_reads(&addrs, 200, 16);
        // зазор 220 > merge_gap 16 — два блока; даже если бы зазор позволял,
        // длина 222 > max_read_len разбила бы пакет
        assert_eq!(plan.len(), 2);
    }
}
