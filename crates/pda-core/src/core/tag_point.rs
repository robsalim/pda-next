//! Объект-модель тега в оперативной памяти.
//!
//! TagPoint хранит ТЕКУЩЕЕ состояние сигнала: конфигурацию, значение,
//! качество, метку времени, версию изменения. Всё изменяемое — под одним
//! мьютексом, поэтому точка безопасно разделяется между потоками опроса,
//! журнала и GUI через Arc. Один цикл: опрос драйвером (сотни Гц) обновляет
//! точку, журнал регистрирует её по своему интервалу + COV, GUI читает не
//! чаще 20 Гц.

use std::collections::VecDeque;

use chrono::{DateTime, Local};
use parking_lot::Mutex;

use super::super::models::Tag;

/// Специальные значения качества.
pub const QUALITY_GOOD: i32 = 0;
pub const QUALITY_BAD: i32 = 1;

/// Максимум незаписанных переходов на тег. deque(maxlen) вытесняет старейшие,
/// защищая память и очередь писателя от перегрузки при экстремальном дребезге.
pub const MAX_PENDING_SAMPLES: usize = 8;

/// Один отсчёт: (метка времени, значение, качество).
pub type Sample = (DateTime<Local>, f64, i32);

/// Всё изменяемое состояние точки.
struct PointState {
    // Конфигурация (обновляется при reload конфигурации)
    name: String,
    group_name: String,
    address_str: String,
    data_type: String,
    unit: String,
    scale: f64,
    offset_val: f64,
    log_interval_ms: i64,
    deadband: f64,
    // Текущее состояние (нет данных = None)
    value: Option<f64>,
    raw_value: Option<f64>,
    quality: i32,
    timestamp: Option<DateTime<Local>>,
    /// Растёт при каждом реальном изменении значения/качества
    version: i64,
    // Состояние регистрации в журнале
    last_logged_value: Option<f64>,
    last_logged_quality: Option<i32>,
    last_logged_ts: Option<DateTime<Local>>,
    /// Переходы, ещё не отданные журналу.
    pending: VecDeque<Sample>,
}

/// Живое состояние одного сигнала. tag_id/connection_id неизменны.
pub struct TagPoint {
    pub tag_id: i64,
    pub connection_id: i64,
    state: Mutex<PointState>,
}

impl TagPoint {
    pub fn from_tag(tag: &Tag) -> Self {
        Self {
            tag_id: tag.id,
            connection_id: tag.connection_id,
            state: Mutex::new(state_from_tag(tag)),
        }
    }

    /// Алиас id тега.
    pub fn id(&self) -> i64 {
        self.tag_id
    }

    // ------------------------------------------------------------------ config
    pub fn name(&self) -> String {
        self.state.lock().name.clone()
    }

    pub fn group_name(&self) -> String {
        self.state.lock().group_name.clone()
    }

    pub fn address_str(&self) -> String {
        self.state.lock().address_str.clone()
    }

    pub fn data_type(&self) -> String {
        self.state.lock().data_type.clone()
    }

    pub fn unit(&self) -> String {
        self.state.lock().unit.clone()
    }

    pub fn scale(&self) -> f64 {
        self.state.lock().scale
    }

    pub fn offset_val(&self) -> f64 {
        self.state.lock().offset_val
    }

    pub fn deadband(&self) -> f64 {
        self.state.lock().deadband
    }

    /// Перенос конфигурации из обновлённого тега, сохраняя живое значение.
    pub fn apply_tag_config(&self, tag: &Tag) {
        let mut st = self.state.lock();
        st.name = tag.name.clone();
        st.group_name = norm_group(&tag.group_name);
        st.address_str = tag.address_str.clone();
        st.data_type = tag.data_type.clone();
        st.unit = tag.unit.clone();
        st.scale = tag.scale;
        st.offset_val = tag.offset_val;
        st.log_interval_ms = tag.log_interval_ms;
        st.deadband = tag.deadband;
    }

    // ------------------------------------------------------------------ update
    /// Записывает свежее показание. Возвращает true, если значение изменилось.
    pub fn update(
        &self,
        raw_value: Option<f64>,
        value: Option<f64>,
        quality: i32,
        ts: DateTime<Local>,
    ) -> bool {
        let mut st = self.state.lock();
        let mut changed = false;
        if quality != st.quality {
            st.quality = quality;
            changed = true;
        }
        let value = match value {
            None => {
                // Сигнал пропал — фиксируем качество, значение оставляем прежним
                return changed;
            }
            Some(v) => v,
        };
        if st.value.is_none() {
            changed = true;
        } else {
            let prev = st.value.unwrap();
            let delta = (value - prev).abs();
            if quality != QUALITY_GOOD || (delta > 0.0 && delta > st.deadband) {
                changed = true;
            }
        }
        // Журнал выбирается реже опроса: запоминаем изменения значения,
        // чтобы короткий импульс (100->101->100 между сканами) не потерялся.
        // Мёртвую зону отсекает track_sample (накопленный дрейф пишется).
        let track = st.value.map_or(true, |prev| value != prev);
        if track {
            track_sample(&mut st, ts, value, quality);
        }
        st.raw_value = raw_value;
        st.value = Some(value);
        st.timestamp = Some(ts);
        if changed {
            st.version += 1;
        }
        changed
    }

    /// Неотданные журналом значения без очистки (для диагностики).
    pub fn pending_samples(&self) -> Vec<Sample> {
        self.state.lock().pending.iter().cloned().collect()
    }

    /// Забирает все накопленные переходы для передачи в журнал.
    pub fn drain_samples(&self) -> Vec<Sample> {
        let mut st = self.state.lock();
        st.pending.drain(..).collect()
    }

    // -------------------------------------------------------------------- log
    /// Нужно ли зарегистрировать точку сейчас.
    ///
    /// Основание: накопились необработанные переходы — каждый импульс между
    /// сканами журнала будет записан отдельной точкой; либо первая точка,
    /// смена качества, либо heartbeat по log_interval_ms.
    pub fn due_for_log(&self, now: DateTime<Local>) -> bool {
        let st = self.state.lock();
        if !st.pending.is_empty() {
            return true;
        }
        if st.value.is_none() {
            return false;
        }
        if st.last_logged_ts.is_none() {
            return true;
        }
        if st.quality != st.last_logged_quality.unwrap_or(QUALITY_BAD) {
            return true;
        }
        // Периодическая регистрация («heartbeat»), если задан интервал
        if st.log_interval_ms > 0 {
            if let Some(prev) = st.last_logged_ts {
                let elapsed = (now - prev).num_milliseconds();
                if elapsed >= st.log_interval_ms {
                    return true;
                }
            }
        }
        false
    }

    /// Отмечает точку зарегистрированной. Если sample передан — фиксирует его
    /// как последнее записанное значение/качество.
    pub fn mark_logged(&self, now: DateTime<Local>, sample: Option<&Sample>) {
        let mut st = self.state.lock();
        match sample {
            Some(s) => {
                st.last_logged_value = Some(s.1);
                st.last_logged_quality = Some(s.2);
            }
            None => {
                st.last_logged_value = st.value;
                st.last_logged_quality = Some(st.quality);
            }
        }
        st.last_logged_ts = Some(now);
    }

    /// Помечает плохим качеством (сигнал не прочитан в этом цикле опроса).
    /// Возвращает true, если состояние действительно изменилось.
    pub fn set_bad(&self) -> bool {
        let mut st = self.state.lock();
        if st.quality != QUALITY_BAD {
            st.quality = QUALITY_BAD;
            st.version += 1;
            return true;
        }
        false
    }

    /// Текущая тройка (timestamp, value, quality) — для снимков реестра.
    pub fn live_triple(&self) -> (Option<DateTime<Local>>, Option<f64>, i32) {
        let st = self.state.lock();
        (st.timestamp, st.value, st.quality)
    }

    /// Нужен ли первый mark_logged для ещё ни разу не зарегистрированной точки.
    pub fn needs_first_mark(&self) -> bool {
        let st = self.state.lock();
        st.last_logged_ts.is_none() && st.value.is_some()
    }

    /// Снимок текущего состояния для читателей (GUI/веб).
    pub fn snapshot(&self) -> Snapshot {
        let st = self.state.lock();
        Snapshot {
            tag_id: self.tag_id,
            connection_id: self.connection_id,
            name: st.name.clone(),
            group_name: st.group_name.clone(),
            address: st.address_str.clone(),
            data_type: st.data_type.clone(),
            unit: st.unit.clone(),
            value: st.value,
            raw: st.raw_value,
            quality: st.quality,
            timestamp: st.timestamp,
            version: st.version,
        }
    }
}

/// Легковесный снимок точки (копия) для GUI/веба.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub tag_id: i64,
    pub connection_id: i64,
    pub name: String,
    pub group_name: String,
    pub address: String,
    pub data_type: String,
    pub unit: String,
    pub value: Option<f64>,
    pub raw: Option<f64>,
    pub quality: i32,
    pub timestamp: Option<DateTime<Local>>,
    pub version: i64,
}

fn state_from_tag(tag: &Tag) -> PointState {
    PointState {
        name: tag.name.clone(),
        group_name: norm_group(&tag.group_name),
        address_str: tag.address_str.clone(),
        data_type: tag.data_type.clone(),
        unit: tag.unit.clone(),
        scale: tag.scale,
        offset_val: tag.offset_val,
        log_interval_ms: tag.log_interval_ms,
        deadband: tag.deadband,
        value: None,
        raw_value: None,
        quality: QUALITY_BAD,
        timestamp: None,
        version: 0,
        last_logged_value: None,
        last_logged_quality: None,
        last_logged_ts: None,
        pending: VecDeque::new(),
    }
}

fn norm_group(g: &str) -> String {
    if g.is_empty() { "Общие".into() } else { g.to_string() }
}

/// Кладёт показание в буфер журнала. Пропускает повтор подряд и дрейф
/// в пределах deadband (от последней точки буфера, а если он пуст — от
/// последней зарегистрированной). Иначе буфер забился бы шумом аналоговых
/// сигналов на 10 000 тегов.
fn track_sample(st: &mut PointState, ts: DateTime<Local>, value: f64, quality: i32) {
    let (anchor_val, anchor_q) = if st.pending.is_empty() {
        (st.last_logged_value, st.last_logged_quality)
    } else {
        let last = st.pending.back().unwrap();
        if value == last.1 && quality == last.2 {
            return;
        }
        (Some(last.1), Some(last.2))
    };

    if let (Some(av), Some(aq)) = (anchor_val, anchor_q) {
        if quality == aq && st.deadband > 0.0 && (value - av).abs() <= st.deadband {
            return;
        }
    }

    // VecDeque с ограничением длины: вытесняем старейший переход
    if st.pending.len() >= MAX_PENDING_SAMPLES {
        st.pending.pop_front();
    }
    st.pending.push_back((ts, value, quality));
}
