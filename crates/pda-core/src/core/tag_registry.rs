//! Единый реестр тегов в оперативной памяти.
//!
//! Владелец — DriverManager. Драйверы опроса пишут сюда показания через
//! update_value(), GUI и веб-сервер читают текущие состояния без обращения к
//! истории в БД. Журнал (historian.rs) выбирает из реестра точки, подлежащие
//! регистрации.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Local};
use parking_lot::{Mutex, RwLock};
use std::sync::Arc;

use super::super::models::Tag;
use super::tag_point::TagPoint;

/// Хранит точки в памяти + индексы по подключениям и «грязные» множества.
pub struct TagRegistry {
    points: RwLock<HashMap<i64, Arc<TagPoint>>>,
    by_conn: RwLock<HashMap<i64, HashSet<i64>>>,
    /// Теги, изменившиеся с момента последней выборки (для GUI)
    dirty: Mutex<DirtyState>,
}

struct DirtyState {
    ids: HashSet<i64>,
    /// monotonic-момент последнего изменения (секунды)
    at: f64,
}

impl TagRegistry {
    pub fn new() -> Self {
        Self {
            points: RwLock::new(HashMap::new()),
            by_conn: RwLock::new(HashMap::new()),
            dirty: Mutex::new(DirtyState { ids: HashSet::new(), at: 0.0 }),
        }
    }

    // ------------------------------------------------------------------ build
    /// Полная перезагрузка реестра из БД (при старте и reload конфигурации).
    pub fn replace_all(&self, tags: &[Tag]) {
        let mut new_points: HashMap<i64, Arc<TagPoint>> = HashMap::new();
        let mut new_by_conn: HashMap<i64, HashSet<i64>> = HashMap::new();

        let old = self.points.read();
        for t in tags {
            let point = match old.get(&t.id) {
                None => Arc::new(TagPoint::from_tag(t)),
                // Сохраняем живое значение, обновляем конфигурацию
                Some(existing) => {
                    existing.apply_tag_config(t);
                    existing.clone()
                }
            };
            new_points.insert(t.id, point);
            new_by_conn.entry(t.connection_id).or_default().insert(t.id);
        }
        drop(old);

        *self.points.write() = new_points;
        *self.by_conn.write() = new_by_conn;
    }

    /// Удаляет все точки подключения (удалён источник/конфигурация).
    pub fn forget_connection(&self, conn_id: i64) {
        let ids = self.by_conn.write().remove(&conn_id).unwrap_or_default();
        let mut points = self.points.write();
        let mut dirty = self.dirty.lock();
        for tid in ids {
            points.remove(&tid);
            dirty.ids.remove(&tid);
        }
    }

    /// Удаляет точку из реестра (тег удалён из конфигурации).
    pub fn drop(&self, tag_id: i64) {
        self.dirty.lock().ids.remove(&tag_id);
        self.points.write().remove(&tag_id);
        for ids in self.by_conn.write().values_mut() {
            ids.remove(&tag_id);
        }
    }

    // -------------------------------------------------------------------- get
    pub fn get(&self, tag_id: i64) -> Option<Arc<TagPoint>> {
        self.points.read().get(&tag_id).cloned()
    }

    pub fn points_for(&self, conn_id: i64) -> Vec<Arc<TagPoint>> {
        let ids = self.by_conn.read();
        let points = self.points.read();
        ids.get(&conn_id)
            .map(|set| set.iter().filter_map(|i| points.get(i).cloned()).collect())
            .unwrap_or_default()
    }

    pub fn all_points(&self) -> Vec<Arc<TagPoint>> {
        self.points.read().values().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.points.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // ----------------------------------------------------------------- update
    /// Обновляет показание тега; масштабирование применяется по scale/offset.
    pub fn update_value(
        &self,
        tag_id: i64,
        raw_value: Option<f64>,
        quality: i32,
        ts: Option<DateTime<Local>>,
    ) {
        let point = match self.points.read().get(&tag_id) {
            Some(p) => p.clone(),
            None => return,
        };
        let value = raw_value.map(|r| r * point.scale() + point.offset_val());
        let changed = point.update(raw_value, value, quality, ts.unwrap_or_else(Local::now));
        if changed {
            let mut dirty = self.dirty.lock();
            dirty.ids.insert(tag_id);
            dirty.at = now_mono();
        }
    }

    /// Сигналы, не прочитанные в этом цикле, помечаются плохим качеством.
    pub fn mark_bad(&self, tag_ids: impl IntoIterator<Item = i64>) {
        let points = self.points.read();
        let mut dirty = self.dirty.lock();
        for tid in tag_ids {
            if let Some(p) = points.get(&tid) {
                if p.set_bad() {
                    dirty.ids.insert(tid);
                    dirty.at = now_mono();
                }
            }
        }
    }

    // ------------------------------------------------------------------ dirty
    /// Забирает накопившиеся изменения (GUI вызывает не чаще 20 Гц).
    /// Возвращает Arc-ы точек, чтобы читатель не зависел от того, перестроит
    /// ли в этот момент реестр драйвер или конфигурация.
    pub fn take_dirty(&self) -> Vec<(i64, Arc<TagPoint>)> {
        let ids = {
            let mut dirty = self.dirty.lock();
            std::mem::take(&mut dirty.ids)
        };
        let points = self.points.read();
        ids.into_iter().filter_map(|i| points.get(&i).map(|p| (i, p.clone()))).collect()
    }

    /// Текущие значения всех сигналов.
    /// Заменяет собой запрос последних значений из истории — GUI больше не
    /// зависит от скорости записи в БД.
    pub fn live_snapshot(&self) -> Vec<LiveValue> {
        self.points.read().values().map(|p| LiveValue::from_point(p)).collect()
    }

    /// Текущие значения только запрошенных тегов.
    /// Дешевле live_snapshot() при тысячах тегов — используется GUI, чтобы
    /// обновить «время обновления» лишь для видимых строк таблицы.
    pub fn read_values(&self, tag_ids: impl IntoIterator<Item = i64>) -> Vec<LiveValue> {
        let points = self.points.read();
        let mut out = Vec::new();
        for tid in tag_ids {
            if let Some(p) = points.get(&tid) {
                out.push(LiveValue::from_point(p));
            }
        }
        out
    }

    // ----------------------------------------------------------------- journal
    /// Точки, подлежащие регистрации (COV + собственный интервал + мёртвая зона).
    ///
    /// Двойные переходы между сканами (100->101->100) не теряются: журнал
    /// выбирается медленнее опроса, поэтому для изменившихся со прошлого
    /// скана точек возвращаются все промежуточные значения из буфера.
    pub fn collect_due(&self, now: DateTime<Local>) -> Vec<DuePoint> {
        let points: Vec<Arc<TagPoint>> = self.points.read().values().cloned().collect();
        let mut out = Vec::new();
        for point in points {
            let mut samples = point.drain_samples();
            if samples.is_empty() && point.due_for_log(now) {
                // первая точка / смена качества / heartbeat: отдаём текущее значение
                let (ts, value, quality) = point.live_triple();
                if let Some(value) = value {
                    samples.push((ts.unwrap_or(now), value, quality));
                }
            }
            if !samples.is_empty() {
                let last = samples[samples.len() - 1].clone();
                for (ts, value, quality) in samples {
                    out.push(DuePoint {
                        timestamp: ts,
                        tag_id: point.tag_id,
                        value,
                        quality,
                    });
                }
                point.mark_logged(now, Some(&last));
            } else if point.needs_first_mark() {
                point.mark_logged(now, None);
            }
        }
        out
    }
}

impl Default for TagRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Текущее состояние тега для читателей.
#[derive(Debug, Clone)]
pub struct LiveValue {
    pub tag_id: i64,
    pub timestamp: Option<DateTime<Local>>,
    pub value: Option<f64>,
    pub quality: i32,
}

impl LiveValue {
    fn from_point(p: &TagPoint) -> Self {
        let (timestamp, value, quality) = p.live_triple();
        Self { tag_id: p.tag_id, timestamp, value, quality }
    }
}

/// Точка, подлежащая регистрации в истории.
#[derive(Debug, Clone, Copy)]
pub struct DuePoint {
    pub timestamp: DateTime<Local>,
    pub tag_id: i64,
    pub value: f64,
    pub quality: i32,
}

/// Монотонное время в секундах.
pub fn now_mono() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// QUALITY_* реэкспортируются для удобства потребителей реестра.
pub use super::tag_point::QUALITY_GOOD;
