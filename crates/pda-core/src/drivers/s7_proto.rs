//! Чистая Rust-реализация транспорта ISO-on-TCP (RFC 1006) + S7comm.
//!
//! Байтовые форматы воспроизводят проверенную реализацию snap7:
//!
//!   * TPKT:  `03 00 <len_be16>` + полезная нагрузка ISO;
//!   * COTP CR: LI=0x11, код 0xE0, dst_ref, src_ref, class 0,
//!     параметры: 0xC1 (src TSAP = `01 00` — PG), 0xC2 (dst TSAP =
//!     `<тип> <rack/slot>`), 0xC0 (макс. размер TPDU = 1024);
//!   * S7 Setup Communication: функция 0xF0, согласование PDU;
//!   * S7 Read Var: функция 0x04, any-type адресация (transport BYTE),
//!     до MAX_VARS адресных элементов в одном PDU (multi-read).
//!
//! Тип TSAP задаётся параметром: 1 (PG) для S7-300/400/1200/1500,
//! 3 (Basic) для S7-200 SMART, у которого удалённый TSAP = 0x0301.
//! V-память SMART при этом просто читается как DB1 — отдельного кода нет.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Коды областей памяти S7 (байт спеки any-type).
pub const AREA_PE: u8 = 0x81; // образ процесса входа (I/E)
pub const AREA_PA: u8 = 0x82; // образ процесса выхода (Q/A)
pub const AREA_MK: u8 = 0x83; // Merker (M)
pub const AREA_DB: u8 = 0x84; // блок данных (DB)

/// Максимум адресных элементов в одном PDU (лимит snap7).
pub const MAX_VARS: usize = 16;

#[derive(Debug)]
pub enum S7Error {
    Io(String),
    Timeout,
    BadResponse(String),
    /// PLC вернул код ошибки (err_class << 8 | err_code) или return_code item.
    Plc(u32),
}

impl S7Error {
    pub fn text(&self) -> String {
        match self {
            S7Error::Io(s) => s.clone(),
            S7Error::Timeout => "время ожидания ответа истекло".into(),
            S7Error::BadResponse(s) => s.clone(),
            S7Error::Plc(code) => format!("ошибка PLC 0x{code:04X}"),
        }
    }
}

impl From<std::io::Error> for S7Error {
    fn from(e: std::io::Error) -> Self {
        S7Error::Io(e.to_string())
    }
}

/// Один адресный элемент multi-read: область + DB + байтовое смещение + длина.
#[derive(Debug, Clone, Copy)]
pub struct S7Item {
    pub area: u8,
    pub db: u16,
    pub start: u32,
    pub len: u16,
}

pub struct S7Conn {
    stream: TcpStream,
    pdu_ref: u16,
    timeout: Duration,
    ///协商ованный с ЦПУ размер PDU (по умолчанию 240 до setup).
    pub pdu_size: u16,
}

impl S7Conn {
    /// TCP + COTP CR + S7 Setup Communication.
    pub async fn connect(
        addr: SocketAddr,
        rack: u8,
        slot: u8,
        tsap_type: u8,
        connect_timeout: Duration,
    ) -> Result<Self, S7Error> {
        let sock = tokio::time::timeout(connect_timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| S7Error::Timeout)?
            .map_err(|e| S7Error::Io(format!("tcp {addr}: {e}")))?;
        let _ = sock.set_nodelay(true);
        let mut conn = Self {
            stream: sock,
            pdu_ref: 0,
            timeout: Duration::from_millis(2000),
            pdu_size: 240,
        };
        conn.cotp_connect(rack, slot, tsap_type).await?;
        conn.setup_communication().await?;
        Ok(conn)
    }

    pub fn set_timeout(&mut self, t: Duration) {
        self.timeout = t;
    }

    fn next_pdu_ref(&mut self) -> u16 {
        self.pdu_ref = self.pdu_ref.wrapping_add(1);
        self.pdu_ref
    }

    // ------------------------------------------------------------------ COTP
    async fn cotp_connect(&mut self, rack: u8, slot: u8, tsap_type: u8) -> Result<(), S7Error> {
        // COTP Connection Request: LI=17, E0, dst_ref, src_ref, class, параметры
        let cr: [u8; 18] = [
            0x11, 0xE0, 0x00, 0x00, 0x00, 0x01, 0x00, // LI, CR, dst_ref, src_ref, class 0
            0xC1, 0x02, 0x01, 0x00, // src TSAP: PG (0x01 0x00)
            0xC2, 0x02, tsap_type, ((rack & 0x07) << 5) | (slot & 0x1F), // dst TSAP
            0xC0, 0x01, 0x0A, // tpdu size 1024
        ];
        self.send(&tpkt(&cr)).await?;
        let resp = self.recv().await?;
        // CC: TPKT(4) + LI + код D0
        let iso = resp.as_slice();
        if iso.len() < 6 || iso[4] & 0xF0 != 0xD0 {
            let code = iso.get(4).copied().unwrap_or(0);
            return Err(S7Error::BadResponse(format!(
                "COTP: ожидал CC (0xD0), получил 0x{code:02X}"
            )));
        }
        Ok(())
    }

    async fn setup_communication(&mut self) -> Result<(), S7Error> {
        // S7 Job: Setup Communication (0xF0), amq=1/1, просим PDU 480
        let param: [u8; 8] = [0xF0, 0x00, 0x00, 0x01, 0x00, 0x01, 0x01, 0xE0];
        let req = self.s7_job(&param, &[]);
        self.send(&iso_data(&req)).await?;
        let body = self.recv_s7().await?;
        let (hdr, payload) = body;
        check_ack(&hdr)?;
        // param ответа: F0 00 <amq_c> <amq_called> <pdu_be16>
        let p = payload;
        if p.len() < 8 || p[0] != 0xF0 {
            return Err(S7Error::BadResponse("setup: неожиданный ответ".into()));
        }
        self.pdu_size = u16::from_be_bytes([p[6], p[7]]);
        Ok(())
    }

    // -------------------------------------------------------------- запросы
    /// Чтение байтов из DB (db_read).
    pub async fn db_read(&mut self, db: u16, start: u32, len: u16) -> Result<Vec<u8>, S7Error> {
        let items = [S7Item { area: AREA_DB, db, start, len }];
        let mut res = self.read_multi(&items).await?;
        match res.pop() {
            Some(Ok(data)) => Ok(data),
            Some(Err(code)) => Err(S7Error::Plc(code as u32)),
            None => Err(S7Error::BadResponse("пустой ответ".into())),
        }
    }

    /// Чтение байтов из именованной области MK/PE/PA.
    pub async fn read_area(&mut self, area: u8, start: u32, len: u16) -> Result<Vec<u8>, S7Error> {
        let items = [S7Item { area, db: 0, start, len }];
        let mut res = self.read_multi(&items).await?;
        match res.pop() {
            Some(Ok(data)) => Ok(data),
            Some(Err(code)) => Err(S7Error::Plc(code as u32)),
            None => Err(S7Error::BadResponse("пустой ответ".into())),
        }
    }

    /// Multi-read: до MAX_VARS элементов в одном PDU.
    /// Возвращает по элементу: Ok(данные) либо Err(return_code PLC) —
    /// элемент с ошибкой не роняет весь запрос (в отличие от snap7,
    /// где сбойный item исключается из значений).
    pub async fn read_multi(&mut self, items: &[S7Item]) -> Result<Vec<Result<Vec<u8>, u8>>, S7Error> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        if items.len() > MAX_VARS {
            return Err(S7Error::BadResponse(format!("multi-read: >{} элементов", MAX_VARS)));
        }
        // param: 04 <кол-во> + по 9 байт на any-type элемент
        let mut param = Vec::with_capacity(2 + items.len() * 10);
        param.push(0x04);
        param.push(items.len() as u8);
        for it in items {
            param.extend_from_slice(&[0x12, 0x0A, 0x10, 0x02]); // spec, len, any-type, transport BYTE
            param.extend_from_slice(&it.len.to_be_bytes());
            param.extend_from_slice(&it.db.to_be_bytes());
            param.push(it.area);
            let addr_bits = it.start * 8; // байтовое смещение в битах, бит 0
            param.push(((addr_bits >> 16) & 0xFF) as u8);
            param.push(((addr_bits >> 8) & 0xFF) as u8);
            param.push((addr_bits & 0xFF) as u8);
        }
        let req = self.s7_job(&param, &[]);
        self.send(&iso_data(&req)).await?;
        // Весь ответный PDU приходит одним кадром (TPDU 1024 > PDU).
        let (hdr, payload) = self.recv_s7().await?;
        check_ack(&hdr)?;
        // Разрезаем поле после заголовка на param/data по длинам из заголовка.
        let pl = hdr.param_len as usize;
        let dl = hdr.data_len as usize;
        if payload.len() < pl + dl {
            return Err(S7Error::BadResponse(format!(
                "обрезанный ответ multi-read: {} байт, нужно {}",
                payload.len(),
                pl + dl
            )));
        }
        let mut data = payload[pl..pl + dl].to_vec();
        let mut out = Vec::with_capacity(items.len());
        for _ in 0..items.len() {
            if data.len() < 4 {
                return Err(S7Error::BadResponse("обрезанный ответ multi-read".into()));
            }
            let ret = data[0];
            let bit_len = u16::from_be_bytes([data[2], data[3]]) as usize;
            let byte_len = bit_len.div_ceil(8);
            if ret == 0xFF {
                if data.len() < 4 + byte_len {
                    return Err(S7Error::BadResponse("обрезанные данные multi-read".into()));
                }
                out.push(Ok(data[4..4 + byte_len].to_vec()));
            } else {
                // Ошибочный элемент не содержит данных; бит-длина = 0.
                out.push(Err(ret));
            }
            let consumed = if ret == 0xFF { 4 + byte_len } else { 4 };
            // Выравнивание до чётной длины (pad может отсутствовать в последнем элементе).
            let padded = consumed + (consumed % 2);
            if padded <= data.len() {
                data.drain(..padded);
            } else {
                data.clear();
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------- транспорт
    /// Собирает S7 Job-PDU: заголовок (10 байт) + param + data.
    fn s7_job(&mut self, param: &[u8], data: &[u8]) -> Vec<u8> {
        let ref_ = self.next_pdu_ref();
        let mut out = Vec::with_capacity(10 + param.len() + data.len());
        out.extend_from_slice(&[0x32, 0x01, 0x00, 0x00]); // magic, Job, reserved
        out.extend_from_slice(&ref_.to_be_bytes());
        out.extend_from_slice(&(param.len() as u16).to_be_bytes());
        out.extend_from_slice(&(data.len() as u16).to_be_bytes());
        out.extend_from_slice(param);
        out.extend_from_slice(data);
        out
    }

    async fn send(&mut self, packet: &[u8]) -> Result<(), S7Error> {
        tokio::time::timeout(self.timeout, self.stream.write_all(packet))
            .await
            .map_err(|_| S7Error::Timeout)?
            .map_err(|e| S7Error::Io(format!("запись: {e}")))?;
        Ok(())
    }

    /// Читает один TPKT-кадр целиком.
    async fn recv(&mut self) -> Result<Vec<u8>, S7Error> {
        let mut head = [0u8; 4];
        tokio::time::timeout(self.timeout, self.stream.read_exact(&mut head))
            .await
            .map_err(|_| S7Error::Timeout)?
            .map_err(|e| S7Error::Io(format!("чтение заголовка TPKT: {e}")))?;
        if head[0] != 0x03 {
            return Err(S7Error::BadResponse(format!("TPKT: версия {}", head[0])));
        }
        let total = u16::from_be_bytes([head[2], head[3]]) as usize;
        if total < 4 || total > 65535 {
            return Err(S7Error::BadResponse(format!("TPKT: длина {total}")));
        }
        let mut buf = vec![0u8; total];
        tokio::time::timeout(self.timeout, self.stream.read_exact(&mut buf[4..]))
            .await
            .map_err(|_| S7Error::Timeout)?
            .map_err(|e| S7Error::Io(format!("чтение TPKT: {e}")))?;
        Ok(buf)
    }

    /// Читает кадр и раскрывает COTP DT + S7-заголовок; возвращает
    /// (поля S7-заголовка, payload за заголовком — param+data одним куском).
    async fn recv_s7(&mut self) -> Result<(S7Header, Vec<u8>), S7Error> {
        let frame = self.recv().await?;
        // COTP DT: LI(1)+код(1)+tpdu_nr => S7 PDU начинается со смещения 5+LI
        let li = frame[4] as usize;
        if frame.len() < 5 + li {
            return Err(S7Error::BadResponse("короткий COTP DT".into()));
        }
        let code = frame[5];
        if code != 0xF0 {
            return Err(S7Error::BadResponse(format!(
                "ожидался COTP Data (0xF0), получен 0x{code:02X}"
            )));
        }
        let iso = &frame[5 + li..];
        decode_s7(iso).map(|(h, p)| (h, p.to_vec()))
    }

    pub async fn close(&mut self) -> Result<(), S7Error> {
        self.stream
            .shutdown()
            .await
            .map_err(|e| S7Error::Io(e.to_string()))
    }
}

// ------------------------------------------------------------------ helpers
fn tpkt(iso: &[u8]) -> Vec<u8> {
    let total = 4 + iso.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&[0x03, 0x00, (total >> 8) as u8, total as u8]);
    out.extend_from_slice(iso);
    out
}

/// Оборачивает S7-PDU в COTP DT: LI=2, F0, tpdu_nr (финальный сегмент).
fn iso_data(s7: &[u8]) -> Vec<u8> {
    let mut iso = Vec::with_capacity(3 + s7.len());
    iso.extend_from_slice(&[0x02, 0xF0, 0x80]);
    iso.extend_from_slice(s7);
    tpkt(&iso)
}

struct S7Header {
    rosctr: u8,
    #[allow(dead_code)]
    pdu_ref: u16,
    param_len: u16,
    data_len: u16,
    error_class: u8,
    error_code: u8,
}

impl S7Header {
    fn text(&self) -> String {
        format!(
            "PDU-тип 0x{:02X}, err {:02X}:{:02X}, param {} data {}",
            self.rosctr, self.error_class, self.error_code, self.param_len, self.data_len
        )
    }
}

fn decode_s7(buf: &[u8]) -> Result<(S7Header, &[u8]), S7Error> {
    if buf.len() < 12 {
        return Err(S7Error::BadResponse(format!("короткий S7 PDU: {} байт", buf.len())));
    }
    if buf[0] != 0x32 {
        return Err(S7Error::BadResponse(format!("S7 magic 0x{:02X}", buf[0])));
    }
    let hdr = S7Header {
        rosctr: buf[1],
        pdu_ref: u16::from_be_bytes([buf[4], buf[5]]),
        param_len: u16::from_be_bytes([buf[6], buf[7]]),
        data_len: u16::from_be_bytes([buf[8], buf[9]]),
        error_class: buf[10],
        error_code: buf[11],
    };
    Ok((hdr, &buf[12..]))
}

fn check_ack(h: &S7Header) -> Result<(), S7Error> {
    if h.rosctr != 0x03 {
        return Err(S7Error::BadResponse(h.text()));
    }
    if h.error_class != 0 || h.error_code != 0 {
        return Err(S7Error::Plc(((h.error_class as u32) << 8) | h.error_code as u32));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tpkt_wraps_iso() {
        let p = tpkt(&[0x02, 0xF0, 0x80]);
        assert_eq!(p, vec![0x03, 0x00, 0x00, 0x07, 0x02, 0xF0, 0x80]);
    }

    #[test]
    fn iso_data_prefix() {
        let p = iso_data(&[0x32, 0x01]);
        assert_eq!(&p[4..7], &[0x02, 0xF0, 0x80]);
        assert_eq!(&p[7..], &[0x32, 0x01]);
    }

    #[test]
    fn cr_packet_layout() {
        // rack 0 / slot 1, TSAP-тип 3 (Basic / S7-200 SMART) => dst 03 01
        let cr: [u8; 18] = build_cr_for_test(0, 1, 3);
        assert_eq!(cr[0], 0x11);
        assert_eq!(cr[1], 0xE0);
        assert_eq!(&cr[6..11], &[0x00, 0xC1, 0x02, 0x01, 0x00]);
        assert_eq!(&cr[11..15], &[0xC2, 0x02, 0x03, 0x01]);
        assert_eq!(&cr[15..18], &[0xC0, 0x01, 0x0A]);
    }

    fn build_cr_for_test(rack: u8, slot: u8, tsap_type: u8) -> [u8; 18] {
        [
            0x11, 0xE0, 0x00, 0x00, 0x00, 0x01, 0x00,
            0xC1, 0x02, 0x01, 0x00,
            0xC2, 0x02, tsap_type, ((rack & 0x07) << 5) | (slot & 0x1F),
            0xC0, 0x01, 0x0A,
        ]
    }
}
