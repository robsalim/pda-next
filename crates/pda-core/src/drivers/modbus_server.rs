//! Драйвер Modbus TCP Server.
//!
//! Приложение само становится Modbus-слинком: внешние Master'ы пишут в наши
//! карты памяти (FC05/06/15/16), а мы декодируем затронутые теги и публикуем
//! их в реестр с меткой времени прихода посылки. Протокол MBAP разворачивается
//! вручную — коды исключений и границы пакетов совпадают с оригиналом.
//!
//! Сервер эмулирует только записываемые области: Holding Registers и Coils.
//! Input Registers и Discrete Inputs наполнять нечем (записи в них нет),
//! поэтому тег в такой области — ошибка конфигурации.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use chrono::{DateTime, Local};
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::core::tag_point::TagPoint;
use crate::core::tag_registry::{now_mono, TagRegistry};
use crate::models::{cfg_i64, cfg_str, Connection};

use super::base::{address_json, submit_values, DriverState};
use super::modbus_address::{
    parse_modbus_address, validate_modbus_address, AREA_COIL, AREA_HOLDING, MODBUS_AREAS_SERVER,
};

pub const ADDRESS_HINT: &str = "Адрес Modbus (сервер): 100 (HR), HR100, C7";

/// Примеры для конструктора в интерфейсе: (адрес, тип, описание).
pub const ADDRESS_EXAMPLES: [(&str, &str, &str); 4] = [
    ("0", "INT16", "Holding register №0 (число без префикса = HR)"),
    ("HR100", "UINT16", "Holding register №100 (FC03/06/16)"),
    ("HR40001", "FLOAT", "Занимает 2 регистра: HR40001 и HR40002"),
    ("C7", "BOOL", "Coil №7 (FC01/05/15)"),
];

// Коды исключений Modbus
const EX_ILLEGAL_FUNCTION: u8 = 0x01;
const EX_ILLEGAL_ADDRESS: u8 = 0x02;
const EX_ILLEGAL_DATA_VALUE: u8 = 0x03;

/// Подпись адреса для диалога переменной.
pub fn address_info() -> serde_json::Value {
    address_json(ADDRESS_HINT, &MODBUS_AREAS_SERVER, &ADDRESS_EXAMPLES)
}

/// Проверка адреса для сервера: общий разбор Modbus + запрет IR/DI.
pub fn validate_address(address: &str, data_type: &str) -> Option<String> {
    if let Some(err) = validate_modbus_address(address, data_type) {
        return Some(err);
    }
    if let Some(a) = parse_modbus_address(address, data_type) {
        if a.area != AREA_HOLDING && a.area != AREA_COIL {
            return Some(format!(
                "Сервер не эмулирует область {}: Input Registers и Discrete Inputs \
                 по Modbus нельзя записать — наполнять их нечем. Доступны Holding \
                 Registers (HR) и Coils (C)",
                a.area
            ));
        }
    }
    None
}

// ------------------------------------------------------------------- память
/// Две карты памяти записываемых областей сервера.
#[derive(Default)]
pub struct ServerMemory {
    pub holding: HashMap<u32, u16>,
    pub coil: HashMap<u32, bool>,
}

/// {(область, смещение): [тег, ...]} — чтобы не читать реестр на каждом пакете.
struct AddrTags {
    ttl: f64,
    at: f64,
    map: HashMap<(String, u32), Vec<Arc<TagPoint>>>,
}

impl AddrTags {
    fn new() -> Self {
        Self { ttl: 2.0, at: 0.0, map: HashMap::new() }
    }
}

/// Декодирует raw-значение тега из текущей карты памяти его области.
pub fn decode(tag: &TagPoint, mem: &ServerMemory) -> Option<f64> {
    let a = parse_modbus_address(&tag.address_str(), &tag.data_type())?;
    let dtype = tag.data_type().to_uppercase();

    if a.is_bit() {
        return Some(if a.area == AREA_COIL {
            *mem.coil.get(&a.offset).unwrap_or(&false) as u8 as f64
        } else {
            0.0
        });
    }

    if dtype == "FLOAT" {
        let r0 = *mem.holding.get(&a.offset).unwrap_or(&0);
        let r1 = *mem.holding.get(&(a.offset + 1)).unwrap_or(&0);
        return Some(f32::from_bits(((r0 as u32) << 16) | r1 as u32) as f64);
    }
    let v = *mem.holding.get(&a.offset).unwrap_or(&0);
    if dtype == "INT16" {
        return Some(v as i16 as f64);
    }
    if dtype == "BOOL" {
        return Some(if v > 0 { 1.0 } else { 0.0 });
    }
    Some(v as f64)
}

/// Перестраивает карту «(область, смещение) -> теги».
/// Регистровые теги перекрывают и второй регистр FLOAT, битовые — только свою
/// ячейку.
fn refresh_addr_tags(cache: &mut AddrTags, registry: &TagRegistry, conn_id: i64) {
    let now = now_mono();
    if !cache.map.is_empty() && (now - cache.at) < cache.ttl {
        return;
    }
    let mut mapping: HashMap<(String, u32), Vec<Arc<TagPoint>>> = HashMap::new();
    for t in registry.points_for(conn_id) {
        let a = match parse_modbus_address(&t.address_str(), &t.data_type()) {
            Some(a) => a,
            None => continue,
        };
        mapping.entry((a.area.clone(), a.offset)).or_default().push(t.clone());
        if !a.is_bit() && t.data_type().to_uppercase() == "FLOAT" {
            // FLOAT занимает второй регистр: запись в него тоже важна
            mapping
                .entry((a.area.clone(), a.offset + 1))
                .or_default()
                .push(t);
        }
    }
    cache.map = mapping;
    cache.at = now;
}

/// После записи карт памяти: декодируем затронутые теги и публикуем их в
/// реестр с меткой времени прихода посылки. Журнал запишет в БД сам, поэтому
/// сетевой цикл не блокируется доступом к базе.
fn apply_write(
    area: &str,
    start_addr: u32,
    reg_cnt: u32,
    rx_timestamp: DateTime<Local>,
    mem: &ServerMemory,
    cache: &mut AddrTags,
    registry: &TagRegistry,
) {
    let mut values: HashMap<i64, f64> = HashMap::new();
    for off in start_addr..start_addr + reg_cnt {
        if let Some(tags) = cache.map.get(&(area.to_string(), off)) {
            for t in tags {
                if let Some(raw) = decode(t, mem) {
                    values.insert(t.tag_id, raw);
                }
            }
        }
    }
    if !values.is_empty() {
        submit_values(registry, &values, rx_timestamp);
    }
}

// ---------------------------------------------------------------- traffic
/// PDU исключения: 0x80|fc + код.
fn exception(fc: u8, code: u8) -> Vec<u8> {
    vec![fc | 0x80, code]
}

/// (byte_count, payload) для ответа FC01/03 либо код исключения.
/// bitwise=true — coils (LSB-first), иначе — holding registers (big-endian).
fn read_cells(mem: &ServerMemory, start: u32, count: u32, bitwise: bool) -> Result<(u8, Vec<u8>), u8> {
    if count < 1 {
        return Err(EX_ILLEGAL_DATA_VALUE);
    }
    let limit = if bitwise { 2000 } else { 125 };
    if count > limit {
        return Err(EX_ILLEGAL_DATA_VALUE);
    }
    if start + count > 0x10000 {
        return Err(EX_ILLEGAL_ADDRESS);
    }

    if bitwise {
        let nbytes = ((count + 7) / 8) as usize;
        let mut data = vec![0u8; nbytes];
        for i in 0..count {
            if *mem.coil.get(&(start + i)).unwrap_or(&false) {
                data[(i / 8) as usize] |= 1 << (i % 8);
            }
        }
        return Ok((nbytes as u8, data));
    }
    let mut payload = Vec::with_capacity(count as usize * 2);
    for i in 0..count {
        let v = *mem.holding.get(&(start + i)).unwrap_or(&0);
        payload.extend_from_slice(&v.to_be_bytes());
    }
    Ok((payload.len() as u8, payload))
}

/// PDU -> ответный PDU. Возвращает пустой вектор, если отвечать не на что.
///
/// Чтение: FC01 (coils) / FC03 (holding). Запись: FC05/15 (coils),
/// FC06/16 (holding). Остальное — Illegal function.
/// `cache` должен быть обновлён вызывающим (handle_client) до входа сюда.
fn handle_pdu(pdu: &[u8], mem: &mut ServerMemory, cache: &mut AddrTags, registry: &TagRegistry, rx: DateTime<Local>) -> Vec<u8> {
    if pdu.is_empty() {
        return Vec::new();
    }
    let fc = pdu[0];

    // ------------------------------------------------------------ чтение
    if fc == 1 || fc == 3 {
        if pdu.len() < 5 {
            return exception(fc, EX_ILLEGAL_DATA_VALUE);
        }
        let start = u16::from_be_bytes([pdu[1], pdu[2]]) as u32;
        let count = u16::from_be_bytes([pdu[3], pdu[4]]) as u32;
        return match read_cells(mem, start, count, fc == 1) {
            Ok((byte_count, payload)) => {
                let mut resp = vec![fc, byte_count];
                resp.extend_from_slice(&payload);
                resp
            }
            Err(code) => exception(fc, code),
        };
    }

    // ------------------------------------------------ FC05: запись катушки
    if fc == 5 {
        if pdu.len() < 5 {
            return exception(fc, EX_ILLEGAL_DATA_VALUE);
        }
        let addr = u16::from_be_bytes([pdu[1], pdu[2]]) as u32;
        let raw = u16::from_be_bytes([pdu[3], pdu[4]]);
        if raw != 0x0000 && raw != 0xFF00 {
            return exception(fc, EX_ILLEGAL_DATA_VALUE);
        }
        let value = raw == 0xFF00;
        mem.coil.insert(addr, value);
        apply_write(AREA_COIL, addr, 1, rx, mem, cache, registry);
        return vec![5, pdu[1], pdu[2], pdu[3], pdu[4]];
    }

    // ------------------------------------------------ FC15: много катушек
    if fc == 15 {
        if pdu.len() < 6 {
            return exception(fc, EX_ILLEGAL_DATA_VALUE);
        }
        let start = u16::from_be_bytes([pdu[1], pdu[2]]) as u32;
        let bit_cnt = u16::from_be_bytes([pdu[3], pdu[4]]) as u32;
        let byte_cnt = pdu[5] as usize;
        let need = 6 + byte_cnt;
        if pdu.len() < need || bit_cnt < 1 || byte_cnt != ((bit_cnt + 7) / 8) as usize {
            return exception(fc, EX_ILLEGAL_DATA_VALUE);
        }
        for i in 0..bit_cnt {
            let bit = pdu[6 + (i / 8) as usize] & (1 << (i % 8)) != 0;
            mem.coil.insert(start + i, bit);
        }
        apply_write(AREA_COIL, start, bit_cnt, rx, mem, cache, registry);
        return vec![15, pdu[1], pdu[2], pdu[3], pdu[4]];
    }

    // ---------------------------------------------- FC16: много регистров
    if fc == 16 {
        if pdu.len() < 6 {
            return exception(fc, EX_ILLEGAL_DATA_VALUE);
        }
        let start = u16::from_be_bytes([pdu[1], pdu[2]]) as u32;
        let reg_cnt = u16::from_be_bytes([pdu[3], pdu[4]]) as u32;
        let byte_cnt = pdu[5] as usize;
        let need = 6 + byte_cnt;
        if pdu.len() < need || reg_cnt < 1 || byte_cnt != (reg_cnt * 2) as usize {
            return exception(fc, EX_ILLEGAL_DATA_VALUE);
        }
        for i in 0..reg_cnt {
            let val = u16::from_be_bytes([pdu[6 + i as usize * 2], pdu[7 + i as usize * 2]]);
            mem.holding.insert(start + i, val);
        }
        apply_write(AREA_HOLDING, start, reg_cnt, rx, mem, cache, registry);
        return vec![16, pdu[1], pdu[2], pdu[3], pdu[4]];
    }

    // ------------------------------------------ FC06: один регистр
    if fc == 6 {
        if pdu.len() < 5 {
            return exception(fc, EX_ILLEGAL_DATA_VALUE);
        }
        let addr = u16::from_be_bytes([pdu[1], pdu[2]]) as u32;
        let val = u16::from_be_bytes([pdu[3], pdu[4]]);
        mem.holding.insert(addr, val);
        apply_write(AREA_HOLDING, addr, 1, rx, mem, cache, registry);
        return vec![6, pdu[1], pdu[2], pdu[3], pdu[4]];
    }

    exception(fc, EX_ILLEGAL_FUNCTION)
}

/// Обслуживание одного подключения: MBAP-заголовок (7 байт) + PDU.
async fn handle_client(
    mut stream: TcpStream,
    conn_id: i64,
    mem: Arc<Mutex<ServerMemory>>,
    cache: Arc<Mutex<AddrTags>>,
    registry: Arc<TagRegistry>,
    state: Arc<DriverState>,
) {
    let mut header = [0u8; 7];
    loop {
        if !state.running.load(Ordering::Relaxed) {
            break;
        }
        // ВАЖНО: время прихода посылки фиксируется сразу, как только прочитан
        // заголовок, — журнал пишет ровно тот момент, когда Master задал значение.
        let rx_timestamp = match read_header(&mut stream, &mut header, &state).await {
            Some(ts) => ts,
            None => break,
        };
        let (_tid, _pid, length, _uid) = (
            u16::from_be_bytes([header[0], header[1]]),
            u16::from_be_bytes([header[2], header[3]]),
            u16::from_be_bytes([header[4], header[5]]),
            header[6],
        );
        if !(2..=260).contains(&length) {
            break;
        }
        let pdu_len = length as usize - 1;
        let mut pdu = vec![0u8; pdu_len];
        if stream.read_exact(&mut pdu).await.is_err() {
            break;
        }

        let resp_pdu = {
            let mut m = mem.lock();
            let mut c = cache.lock();
            refresh_addr_tags(&mut c, &registry, conn_id);
            handle_pdu(&pdu, &mut m, &mut c, &registry, rx_timestamp)
        };
        if resp_pdu.is_empty() {
            continue;
        }

        // MBAP ответа: те же Transaction ID / Protocol ID / Unit ID, своя длина
        let mut resp = Vec::with_capacity(7 + resp_pdu.len());
        resp.extend_from_slice(&header[0..4]); // tid + pid
        resp.extend_from_slice(&((resp_pdu.len() + 1) as u16).to_be_bytes());
        resp.push(header[6]); // unit id
        resp.extend_from_slice(&resp_pdu);
        if stream.write_all(&resp).await.is_err() {
            break;
        }
        let _ = stream.flush().await;
    }
    let _ = stream.shutdown().await;
}

/// Читает 7-байтный заголовок, возвращает время прихода. Таймаут нужен, чтобы
/// остановка драйвера не зависела в idle-соединении.
async fn read_header(stream: &mut TcpStream, buf: &mut [u8; 7], state: &DriverState) -> Option<DateTime<Local>> {
    loop {
        let r = tokio::time::timeout(std::time::Duration::from_millis(500), stream.read_exact(buf)).await;
        match r {
            Ok(Ok(_)) => return Some(Local::now()),
            Ok(Err(_)) => return None,
            Err(_) => {
                if !state.running.load(Ordering::Relaxed) {
                    return None;
                }
            }
        }
    }
}

// ------------------------------------------------------------------- driver
/// Точка входа: слушает `host:port`, принимает подключения (задача на каждое),
/// пока `state.running`. Аналог `start()` + `_run_server()` в Python.
pub async fn run_modbus_server(
    conn_cfg: Arc<Connection>,
    registry: Arc<TagRegistry>,
    state: Arc<DriverState>,
) {
    let port = cfg_i64(&conn_cfg.config, "port", crate::config::DEFAULT_MODBUS_SERVER_PORT as i64)
        .clamp(1, 65535) as u16;
    let host = cfg_str(&conn_cfg.config, "host", "0.0.0.0");
    let bind: SocketAddr = match format!("{host}:{port}").parse() {
        Ok(a) => a,
        Err(e) => {
            state.set_status(format!("Error: неверный адрес '{host}:{port}' ({e})"));
            state.running.store(false, Ordering::SeqCst);
            return;
        }
    };

    let listener = match TcpListener::bind(bind).await {
        Ok(l) => l,
        Err(e) => {
            // Порт 502 требует прав администратора — как в README Python-версии
            state.set_status(format!("Error: {e}"));
            *state.last_error.lock() = Some(e.to_string());
            state.running.store(false, Ordering::SeqCst);
            return;
        }
    };

    let mem = Arc::new(Mutex::new(ServerMemory::default()));
    let cache = Arc::new(Mutex::new(AddrTags::new()));
    state.running.store(true, Ordering::SeqCst);
    state.set_status(format!("Running (Port {port})"));

    loop {
        if !state.running.load(Ordering::Relaxed) {
            break;
        }
        let accepted =
            tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept()).await;
        let stream = match accepted {
            Ok(Ok((s, _peer))) => s,
            Ok(Err(e)) => {
                *state.last_error.lock() = Some(e.to_string());
                continue;
            }
            Err(_) => continue, // idle: проверили флаг stopped
        };
        let conn_id = conn_cfg.id;
        let mem = mem.clone();
        let cache = cache.clone();
        let registry = registry.clone();
        let state = state.clone();
        tokio::spawn(async move {
            handle_client(stream, conn_id, mem, cache, registry, state).await;
        });
    }

    state.set_status("Stopped");
}

// -------------------------------------------------------------------- tests
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Tag;

    fn tag(id: i64, address: &str, data_type: &str) -> Tag {
        Tag::new(id, 1, format!("t{id}"), address, data_type, 1.0, 0.0, "", "Общие", 1000, 0.0)
    }

    /// Реестр + карта адресов из одних и тех же тегов (аналог кэша с ttl=∞).
    fn world(tags: &[Tag]) -> (Arc<TagRegistry>, AddrTags) {
        let reg = Arc::new(TagRegistry::new());
        reg.replace_all(tags);
        let mut map: HashMap<(String, u32), Vec<Arc<TagPoint>>> = HashMap::new();
        for t in tags {
            let a = match parse_modbus_address(&t.address_str, &t.data_type) {
                Some(a) => a,
                None => continue,
            };
            let p = reg.get(t.id).unwrap();
            map.entry((a.area.clone(), a.offset)).or_default().push(p.clone());
            if !a.is_bit() && t.data_type.to_uppercase() == "FLOAT" {
                map.entry((a.area.clone(), a.offset + 1)).or_default().push(p);
            }
        }
        (reg, AddrTags { ttl: f64::INFINITY, at: 0.0, map })
    }

    #[test]
    fn rejects_non_writable_areas() {
        assert!(validate_address("IR200", "INT16").is_some());
        assert!(validate_address("DI7", "BOOL").is_some());
        assert!(validate_address("HR100", "INT16").is_none());
        assert!(validate_address("C7", "BOOL").is_none());
        assert!(validate_address("100", "FLOAT").is_none());
    }

    #[test]
    fn fc16_then_fc03_roundtrip_float() {
        // Сервер работает по сырым PDU-адресам: тег на HR160, FC16 пишет туда же
        let (reg, mut cache) = world(&[tag(1, "HR160", "FLOAT")]);
        let mut mem = ServerMemory::default();

        // Пишем 50.0 (0x42480000) двумя регистрами: адрес 160 (0xA0), 2 регистра, 4 байта
        let mut pdu = vec![16, 0, 160, 0, 2, 4];
        pdu.extend_from_slice(&[0x42, 0x48, 0x00, 0x00]);
        let resp = handle_pdu(&pdu, &mut mem, &mut cache, &reg, Local::now());
        assert_eq!(resp, vec![16, 0, 160, 0, 2]);
        assert_eq!(*mem.holding.get(&160).unwrap(), 0x4248);

        // Читаем обратно FC03
        let read = vec![3, 0, 160, 0, 2];
        let resp = handle_pdu(&read, &mut mem, &mut cache, &reg, Local::now());
        assert_eq!(resp, vec![3, 4, 0x42, 0x48, 0x00, 0x00]);

        // Тег опубликован в реестре со значением 50.0
        let live = reg.read_values([1]);
        assert!((live[0].value.unwrap() - 50.0).abs() < 1e-3);
        assert_eq!(live[0].quality, crate::core::tag_point::QUALITY_GOOD);
    }

    #[test]
    fn fc05_coil_write_and_read() {
        let (reg, mut cache) = world(&[tag(2, "C7", "BOOL")]);
        let mut mem = ServerMemory::default();

        let on = vec![5, 0, 7, 0xFF, 0x00];
        assert_eq!(handle_pdu(&on, &mut mem, &mut cache, &reg, Local::now()), on);
        assert_eq!(*mem.coil.get(&7).unwrap(), true);
        assert_eq!(reg.read_values([2])[0].value, Some(1.0));

        let resp = handle_pdu(&[1, 0, 0, 0, 8], &mut mem, &mut cache, &reg, Local::now());
        assert_eq!(resp, vec![1, 1, 0b1000_0000]);
    }

    #[test]
    fn bad_pdu_values_get_exceptions() {
        let (reg, mut cache) = world(&[]);
        let mut mem = ServerMemory::default();

        // FC05 с невалидным значением (не 0x0000/0xFF00)
        assert_eq!(handle_pdu(&[5, 0, 1, 0x12, 0x34], &mut mem, &mut cache, &reg, Local::now()), vec![0x85, EX_ILLEGAL_DATA_VALUE]);
        // FC03 с count = 0
        assert_eq!(handle_pdu(&[3, 0, 0, 0, 0], &mut mem, &mut cache, &reg, Local::now()), vec![0x83, EX_ILLEGAL_DATA_VALUE]);
        // FC03 с count = 126 (> 125)
        assert_eq!(handle_pdu(&[3, 0, 0, 0, 126], &mut mem, &mut cache, &reg, Local::now()), vec![0x83, EX_ILLEGAL_DATA_VALUE]);
        // Неизвестный функционал
        assert_eq!(handle_pdu(&[23, 0, 0], &mut mem, &mut cache, &reg, Local::now()), vec![0x97, EX_ILLEGAL_FUNCTION]);
        // Короткий PDU
        assert_eq!(handle_pdu(&[3, 0, 0], &mut mem, &mut cache, &reg, Local::now()), vec![0x83, EX_ILLEGAL_DATA_VALUE]);
        // Пустой PDU — не отвечаем
        assert!(handle_pdu(&[], &mut mem, &mut cache, &reg, Local::now()).is_empty());
    }

    #[test]
    fn read_beyond_area_bounds_error() {
        let (reg, mut cache) = world(&[]);
        let mut mem = ServerMemory::default();
        // start+count > 0x10000 — Illegal data address
        let resp = handle_pdu(&[3, 0xFF, 0xFF, 0, 2], &mut mem, &mut cache, &reg, Local::now());
        assert_eq!(resp, vec![0x83, EX_ILLEGAL_ADDRESS]);
    }

    #[test]
    fn fc15_multiple_coils() {
        let (reg, mut cache) = world(&[]);
        let mut mem = ServerMemory::default();
        // старт 0, 10 бит, 2 байта данных: 0b0000_0101, 0b0000_0010
        let resp = handle_pdu(&[15, 0, 0, 0, 10, 2, 0b101, 0b10], &mut mem, &mut cache, &reg, Local::now());
        assert_eq!(resp, vec![15, 0, 0, 0, 10]);
        assert_eq!(*mem.coil.get(&0).unwrap(), true);
        assert_eq!(*mem.coil.get(&1).unwrap(), false);
        assert_eq!(*mem.coil.get(&2).unwrap(), true);
        assert_eq!(*mem.coil.get(&9).unwrap(), true);
    }

    #[test]
    fn fc16_wrong_byte_count_rejected() {
        let (reg, mut cache) = world(&[]);
        let mut mem = ServerMemory::default();
        // reg_cnt=2 требует 4 байта, а в PDU 3 — Illegal data value
        assert_eq!(handle_pdu(&[16, 0, 0, 0, 2, 3, 1, 2, 3], &mut mem, &mut cache, &reg, Local::now()), vec![0x90, EX_ILLEGAL_DATA_VALUE]);
    }
}
