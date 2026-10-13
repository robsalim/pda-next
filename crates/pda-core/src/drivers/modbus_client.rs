//! Драйвер Modbus TCP Client.
//!
//! Транспорт — tokio-modbus (TCP, MBAP). Соседние адреса одной области
//! читаются одним запросом (merge_gap=8), области FC03/FC04/FC01/FC02.
//! Значения публикуем в реестр — масштабирование и журнал делают своё дело.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Local;
use tokio_modbus::client::{tcp as mb_tcp, Context as MbContext, Reader};
use tokio_modbus::Slave;

use crate::core::tag_point::TagPoint;
use crate::core::tag_registry::TagRegistry;
use crate::models::{cfg_i64, cfg_str, Connection};

use super::base::{
    fmt_poll_status, mark_cycle_errors, poll_period, submit_values, update_stats, DriverState,
    TagCache,
};
use super::modbus_address::{
    parse_modbus_address, validate_modbus_address, AREA_COIL, AREA_DISCRETE, AREA_HOLDING,
    AREA_INPUT, MAX_BITS_PER_REQUEST, MAX_REGS_PER_REQUEST,
};

pub const ADDRESS_HINT: &str = "Адрес Modbus: 100 (HR), HR100, IR200, C7, DI15";

/// Примеры для конструктора в интерфейсе: (адрес, тип, описание).
pub const ADDRESS_EXAMPLES: [(&str, &str, &str); 6] = [
    ("0", "INT16", "Holding register №0 (число без префикса = HR)"),
    ("HR100", "UINT16", "Holding register №100"),
    ("HR40001", "FLOAT", "Занимает 2 регистра: HR40001 и HR40002"),
    ("IR200", "UINT16", "Input register №200 (FC04, только чтение)"),
    ("C7", "BOOL", "Coil №7 — дискретный выход (FC01)"),
    ("DI15", "BOOL", "Discrete input №15 — дискретный вход (FC02)"),
];

/// Подпись адреса для диалога переменной.
pub fn address_info() -> serde_json::Value {
    super::base::address_json(ADDRESS_HINT, &super::modbus_address::MODBUS_AREAS, &ADDRESS_EXAMPLES)
}

pub fn validate_address(address: &str, data_type: &str) -> Option<String> {
    validate_modbus_address(address, data_type)
}

fn is_bitwise(area: &str) -> bool {
    matches!(area, AREA_COIL | AREA_DISCRETE)
}

fn area_limit(area: &str) -> usize {
    if is_bitwise(area) {
        MAX_BITS_PER_REQUEST as usize
    } else {
        MAX_REGS_PER_REQUEST as usize
    }
}

/// Клетка карты памяти: слово регистра или бит.
#[derive(Debug, Clone, Copy)]
pub enum Cell {
    Reg(u16),
    Bit(bool),
}

impl Cell {
    pub fn as_u16(self) -> u16 {
        match self {
            Cell::Reg(v) => v,
            Cell::Bit(b) => b as u16,
        }
    }
    pub fn as_bool(self) -> bool {
        match self {
            Cell::Reg(v) => v != 0,
            Cell::Bit(b) => b,
        }
    }
}

// ---------------------------------------------------------------------- план
/// Один блок чтения: (смещение, количество).
pub type Block = (u32, usize);

/// Собирает блоки чтения по областям: соседние адреса одной области читаются
/// одним запросом вместо запроса на каждый тег.
/// Возвращает {область: [(start, count), ...]} в порядке первой встречи области.
pub fn plan_by_area(tags: &[Arc<TagPoint>], merge_gap: u32) -> Vec<(String, Vec<Block>)> {
    let mut needed: BTreeMap<&'static str, Vec<u32>> = BTreeMap::new();
    let mut order: Vec<&'static str> = Vec::new();
    for t in tags {
        let a = match parse_modbus_address(&t.address_str(), &t.data_type()) {
            Some(a) => a,
            None => continue,
        };
        let key: &'static str = match a.area.as_str() {
            AREA_HOLDING => AREA_HOLDING,
            AREA_INPUT => AREA_INPUT,
            AREA_COIL => AREA_COIL,
            _ => AREA_DISCRETE,
        };
        let set = needed.entry(key).or_insert_with(|| {
            order.push(key);
            Vec::new()
        });
        set.push(a.offset);
        if !a.is_bit() && a.size(&t.data_type()) == 2 {
            // FLOAT занимает второй регистр: его тоже надо прочитать
            set.push(a.offset + 1);
        }
    }
    order
        .into_iter()
        .map(|area| {
            let offs = needed.remove(area).unwrap();
            let mut offs: Vec<u32> = {
                let mut s: Vec<u32> = offs;
                s.sort_unstable();
                s.dedup();
                s
            };
            let limit = area_limit(area);
            let mut blocks: Vec<Block> = Vec::new();
            let mut start = offs[0];
            let mut prev = offs[0];
            for &x in &offs[1..] {
                if x <= prev + 1 + merge_gap && (x - start + 1) <= limit as u32 {
                    prev = x;
                    continue;
                }
                blocks.push((start, (prev - start + 1) as usize));
                start = x;
                prev = x;
            }
            blocks.push((start, (prev - start + 1) as usize));
            offs.clear();
            (area.to_string(), blocks)
        })
        .collect()
}

// --------------------------------------------------------------------- reads
async fn read_block(
    ctx: &mut MbContext,
    area: &str,
    start: u32,
    count: usize,
) -> Result<Vec<Cell>, std::io::Error> {
    let addr = start as u16;
    let qty = count as u16;
    let cells = match area {
        AREA_HOLDING => ctx
            .read_holding_registers(addr, qty)
            .await?
            .into_iter()
            .map(Cell::Reg)
            .collect(),
        AREA_INPUT => ctx
            .read_input_registers(addr, qty)
            .await?
            .into_iter()
            .map(Cell::Reg)
            .collect(),
        AREA_COIL => ctx
            .read_coils(addr, qty)
            .await?
            .into_iter()
            .map(Cell::Bit)
            .collect(),
        _ => ctx
            .read_discrete_inputs(addr, qty)
            .await?
            .into_iter()
            .map(Cell::Bit)
            .collect(),
    };
    Ok(cells)
}

/// Читает все блоки всех областей -> {(область, смещение): значение}.
/// Ошибки отдельных блоков мягкие: записываем в last_error и идём дальше.
pub async fn read_blocks(
    ctx: &mut MbContext,
    plan: &[(String, Vec<Block>)],
    req_count: &mut usize,
    last_error: &mut Option<String>,
) -> HashMap<(String, u32), Cell> {
    let mut cells: HashMap<(String, u32), Cell> = HashMap::new();
    for (area, blocks) in plan {
        for &(start, count) in blocks {
            match read_block(ctx, area, start, count).await {
                Ok(vals) => {
                    *req_count += 1;
                    for (i, v) in vals.iter().enumerate().take(count) {
                        cells.insert((area.clone(), start + i as u32), *v);
                    }
                }
                Err(e) => {
                    *last_error = Some(format!("FC{area} {start}+{count}: {e}"));
                }
            }
        }
    }
    cells
}

/// Превращает прочитанную ячейку области в числовое значение тега.
pub fn decode_raw(t: &TagPoint, cells: &HashMap<(String, u32), Cell>) -> Option<f64> {
    let a = parse_modbus_address(&t.address_str(), &t.data_type())?;
    let dtype = t.data_type().to_uppercase();

    if a.is_bit() {
        let bit = cells.get(&(a.area.clone(), a.offset))?;
        return Some(if bit.as_bool() { 1.0 } else { 0.0 });
    }

    let v = cells.get(&(a.area.clone(), a.offset))?.as_u16();
    if dtype == "FLOAT" {
        let v2 = cells.get(&(a.area.clone(), a.offset + 1))?.as_u16();
        let bits = (v as u32) << 16 | v2 as u32;
        return Some(f32::from_bits(bits) as f64);
    }
    if dtype == "INT16" {
        return Some(v as i16 as f64);
    }
    if dtype == "BOOL" {
        return Some(if v > 0 { 1.0 } else { 0.0 });
    }
    Some(v as f64)
}

// -------------------------------------------------------------------- driver
/// Точка входа: цикл опроса одного подключения modbus_client (tokio-задача).
pub async fn run_modbus_client(
    conn_cfg: Arc<Connection>,
    registry: Arc<TagRegistry>,
    state: Arc<DriverState>,
) {
    let ip = cfg_str(&conn_cfg.config, "ip", "127.0.0.1");
    let port = cfg_i64(&conn_cfg.config, "port", 502).clamp(1, 65535) as u16;
    let slave_id = cfg_i64(&conn_cfg.config, "slave_id", 1).clamp(0, 255) as u8;

    let addr = match ip.parse::<IpAddr>() {
        Ok(a) => SocketAddr::new(a, port),
        Err(e) => {
            state.set_status(format!("Connect Error: неверный IP '{ip}' ({e})"));
            state.running.store(false, std::sync::atomic::Ordering::SeqCst);
            return;
        }
    };

    let period = poll_period(&conn_cfg);
    let timeout = period.max(Duration::from_millis(2000)).min(Duration::from_millis(5000));
    let mut cache = TagCache::new();
    let merge_gap: u32 = 8; // разрыв, выгоднее прочитать «заодно»

    state.running.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut ctx: Option<MbContext> = None;

    while state.running.load(std::sync::atomic::Ordering::Relaxed) {
        // ------------------------------------------------ reconnect
        if ctx.is_none() {
            state.set_status("Connecting...");
            let opened = match tokio::time::timeout(timeout, mb_tcp::connect_slave(addr, Slave(slave_id)))
                .await
            {
                Ok(Ok(c)) => Some(c),
                Ok(Err(e)) => {
                    *state.last_error.lock() = Some(e.to_string());
                    None
                }
                Err(_) => {
                    *state.last_error.lock() = Some(format!("TCP connect {addr}: timeout"));
                    None
                }
            };
            match opened {
                Some(c) => {
                    cache.invalidate(); // теги/адреса могли измениться
                    ctx = Some(c);
                    state.set_status("Connected");
                }
                None => {
                    state.set_status("Connect Error");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            }
        }

        // ------------------------------------------------ цикл опроса
        let cycle_start = Instant::now();
        state.last_error.lock().take();
        let mut requests = 0usize;
        let mut last_error: Option<String> = None;

        let t0 = Instant::now();
        let tags = cache.get(&registry, conn_cfg.id);
        let db_ms = t0.elapsed().as_secs_f64();

        let c = ctx.as_mut().expect("есть соединение");
        let plan = plan_by_area(&tags, merge_gap);
        let t_read = Instant::now();
        let cells = read_blocks(c, &plan, &mut requests, &mut last_error).await;
        let read_ms = t_read.elapsed().as_secs_f64();

        // Транспорт умер? (обрыв, таймаут сокета) — в следующем цикле переподключимся
        let transport_fault = last_error
            .as_deref()
            .map(|e| e.contains("timeout") || e.contains("connection") || e.contains("Connection"))
                .unwrap_or(false);

        let now = Local::now();
        let mut values: HashMap<i64, f64> = HashMap::new();
        for t in &tags {
            if let Some(raw) = decode_raw(t, &cells) {
                // submit_values / TagPoint применят scale/offset сами
                values.insert(t.tag_id, raw);
            }
        }
        submit_values(&registry, &values, now);
        let missing: Vec<i64> = tags
            .iter()
            .filter(|t| !values.contains_key(&t.tag_id))
            .map(|t| t.tag_id)
            .collect();
        if !missing.is_empty() {
            mark_cycle_errors(&registry, missing);
        }

        if transport_fault {
            drop(ctx.take()); // соединение «протухло» — переподключимся
        }

        let cycle_s = cycle_start.elapsed().as_secs_f64();
        update_stats(&state, cycle_s, read_ms, db_ms, requests, tags.len());

        if let Some(err) = last_error {
            *state.last_error.lock() = Some(err.clone());
            state.set_status(format!("Poll error: {err}"));
        } else {
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

// ---------------------------------------------------------------------- tests
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Tag;

    fn tag(id: i64, address: &str, data_type: &str) -> Arc<TagPoint> {
        let t = Tag::new(id, 1, format!("t{id}"), address, data_type, 1.0, 0.0, "", "Общие", 1000, 0.0);
        Arc::new(TagPoint::from_tag(&t))
    }

    #[test]
    fn plan_merges_nearby_offsets() {
        let tags = vec![
            tag(1, "100", "INT16"),
            tag(2, "105", "INT16"), // зазор 5 <= 8 — тот же блок
            tag(3, "130", "INT16"), // зазор 25 > 8 — новый блок
            tag(4, "C7", "BOOL"),   // другая область
        ];
        let plan = plan_by_area(&tags, 8);
        let hr = plan.iter().find(|(a, _)| a == AREA_HOLDING).unwrap();
        assert_eq!(hr.1, vec![(100, 6), (130, 1)]);
        let c = plan.iter().find(|(a, _)| a == AREA_COIL).unwrap();
        assert_eq!(c.1, vec![(7, 1)]);
    }

    #[test]
    fn plan_includes_second_float_register() {
        let tags = vec![tag(1, "HR40001", "FLOAT")];
        let plan = plan_by_area(&tags, 8);
        assert_eq!(plan[0].1, vec![(40001, 2)]);
    }

    #[test]
    fn plan_respects_max_regs() {
        let tags = vec![tag(1, "0", "INT16"), tag(2, "130", "INT16")];
        let plan = plan_by_area(&tags, 1000); // большой зазор, но limit 125
        assert_eq!(plan[0].1, vec![(0, 1), (130, 1)]);
    }

    #[test]
    fn decodes_cells() {
        let mut cells: HashMap<(String, u32), Cell> = HashMap::new();
        cells.insert((AREA_HOLDING.to_string(), 10), Cell::Reg(0x4248));
        cells.insert((AREA_HOLDING.to_string(), 11), Cell::Reg(0x0000));
        cells.insert((AREA_HOLDING.to_string(), 12), Cell::Reg(0xFFFF));
        cells.insert((AREA_COIL.to_string(), 7), Cell::Bit(true));

        let t = tag(1, "10", "FLOAT");
        assert!((decode_raw(&t, &cells).unwrap() - 50.0).abs() < 1e-3);
        let t = tag(2, "12", "INT16");
        assert_eq!(decode_raw(&t, &cells).unwrap(), -1.0);
        let t = tag(3, "12", "");
        assert_eq!(decode_raw(&t, &cells).unwrap(), 65535.0);
        let t = tag(4, "C7", "BOOL");
        assert_eq!(decode_raw(&t, &cells).unwrap(), 1.0);
        let t = tag(5, "IR5", "UINT16");
        assert_eq!(decode_raw(&t, &cells), None);
    }
}
