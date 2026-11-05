//! Тесты ядра без БД: TagPoint (COV/deadband/буфер переходов/heartbeat)
//! и TagRegistry (индексы, dirty-списки, масштабирование, collect_due).
//! Внешние сервисы не требуют — всё in-memory.

use chrono::{DateTime, Duration, Local};
use pda_core::core::tag_point::{TagPoint, QUALITY_BAD, QUALITY_GOOD};
use pda_core::core::tag_registry::TagRegistry;
use pda_core::models::Tag;

fn fresh_tag(tag_id: i64, conn_id: i64) -> Tag {
    Tag::new(
        tag_id,
        conn_id,
        "t",
        "HR100",
        "FLOAT",
        1.0,
        0.0,
        "°C",
        "Общие",
        1000,
        0.0,
    )
}

fn now() -> DateTime<Local> {
    Local::now()
}

#[tokio::test]
async fn tag_point_scale_and_offset_applied_on_update() {
    let tag = fresh_tag(1, 1);
    let tp = TagPoint::from_tag(&tag);
    assert_eq!(tp.scale(), 1.0);

    let mut t2 = fresh_tag(2, 1);
    t2.scale = 2.0;
    t2.offset_val = 10.0;
    let tp2 = TagPoint::from_tag(&t2);
    tp2.update(Some(5.0), Some(20.0), QUALITY_GOOD, now());
    let (_, v, q) = tp2.live_triple();
    assert_eq!(v, Some(20.0));
    assert_eq!(q, QUALITY_GOOD);
}

#[tokio::test]
async fn tag_point_quality_change_is_logged() {
    let tag = fresh_tag(1, 1);
    let tp = TagPoint::from_tag(&tag);
    tp.update(Some(10.0), Some(10.0), QUALITY_GOOD, now());
    assert_eq!(tp.pending_samples().len(), 1, "первое значение попадает в pending");
    assert!(tp.due_for_log(now()), "первое значение — повод для логирования");
    tp.mark_logged(now(), Some(&tp.drain_samples()[0]));
    // Значение не меняется (10.0 == 10.0), поэтому в pending не добавляется,
    // но смена качества GOOD -> BAD делает точку пригодной для логирования.
    tp.update(Some(10.0), Some(10.0), QUALITY_BAD, now());
    assert_eq!(tp.pending_samples().len(), 0, "одинаковое значение не дублируется в pending");
    assert!(tp.due_for_log(now()), "смена качества — повод для логирования");
    tp.mark_logged(now(), None);
    assert!(!tp.due_for_log(now()));
}

#[tokio::test]
async fn tag_point_double_impulse_not_lost() {
    let tag = fresh_tag(1, 1);
    let tp = TagPoint::from_tag(&tag);
    let t0 = now();
    let t1 = t0 + Duration::milliseconds(10);
    let t2 = t1 + Duration::milliseconds(10);
    tp.update(Some(100.0), Some(100.0), QUALITY_GOOD, t0);
    tp.update(Some(101.0), Some(101.0), QUALITY_GOOD, t1);
    tp.update(Some(100.0), Some(100.0), QUALITY_GOOD, t2);
    let samples = tp.drain_samples();
    assert_eq!(samples.len(), 3, "все три перехода: {samples:?}");
    assert_eq!(samples[0], (t0, 100.0, QUALITY_GOOD));
    assert_eq!(samples[1], (t1, 101.0, QUALITY_GOOD));
    assert_eq!(samples[2], (t2, 100.0, QUALITY_GOOD));
}

#[tokio::test]
async fn tag_point_deadband_filters_small_drift() {
    let mut tag = fresh_tag(1, 1);
    tag.deadband = 0.5;
    let tp = TagPoint::from_tag(&tag);
    tp.update(Some(10.0), Some(10.0), QUALITY_GOOD, now());
    tp.update(Some(10.2), Some(10.2), QUALITY_GOOD, now()); // 0.2 <= 0.5 — отфильтровано
    tp.update(Some(10.7), Some(10.7), QUALITY_GOOD, now()); // 0.7 > 0.5 — проходит
    let samples = tp.drain_samples();
    assert_eq!(samples.len(), 2, "первое + значимое изменение: {samples:?}");
    assert_eq!(samples[0].1, 10.0);
    assert_eq!(samples[1].1, 10.7);
}

#[tokio::test]
async fn tag_point_heartbeat_by_interval() {
    let mut tag = fresh_tag(1, 1);
    tag.log_interval_ms = 500;
    let tp = TagPoint::from_tag(&tag);
    let t0 = now();
    tp.update(Some(10.0), Some(10.0), QUALITY_GOOD, t0);
    assert!(tp.due_for_log(t0));
    tp.mark_logged(t0, Some(&tp.drain_samples()[0]));
    assert!(!tp.due_for_log(t0 + Duration::milliseconds(400)));
    assert!(tp.due_for_log(t0 + Duration::milliseconds(600)));
}

#[tokio::test]
async fn tag_registry_replace_all_keeps_live_values() {
    let reg = TagRegistry::new();
    let t1 = Tag::new(1, 1, "t1", "HR100", "FLOAT", 1.0, 0.0, "", "Общие", 1000, 0.0);
    reg.replace_all(&[t1]);
    reg.update_value(1, Some(10.0), QUALITY_GOOD, None);
    assert_eq!(reg.get(1).unwrap().live_triple().1, Some(10.0));

    let t1b = Tag::new(1, 1, "t1 renamed", "HR200", "INT16", 2.0, 5.0, "", "Общие", 1000, 0.0);
    reg.replace_all(&[t1b]);
    let p1b = reg.get(1).unwrap();
    assert_eq!(p1b.name(), "t1 renamed");
    assert_eq!(p1b.address_str(), "HR200");
    assert_eq!(p1b.scale(), 2.0);
    assert_eq!(p1b.offset_val(), 5.0);
    assert_eq!(p1b.live_triple().1, Some(10.0));
}

#[tokio::test]
async fn tag_registry_forget_connection_removes_its_tags() {
    let reg = TagRegistry::new();

#[tokio::test]
async fn tag_registry_take_dirty_and_live_snapshot() {
    let reg = TagRegistry::new();
    let t1 = Tag::new(1, 1, "t1", "HR100", "FLOAT", 1.0, 0.0, "", "Общие", 1000, 0.0);
    let t2 = Tag::new(2, 1, "t2", "HR101", "FLOAT", 1.0, 0.0, "", "Общие", 1000, 0.0);
    reg.replace_all(&[t1, t2]);
    assert!(reg.take_dirty().is_empty());
    reg.update_value(1, Some(5.0), QUALITY_GOOD, None);
    let dirty = reg.take_dirty();
    assert_eq!(dirty.len(), 1);
    assert_eq!(dirty[0].0, 1);
    assert_eq!(dirty[0].1.live_triple().1, Some(5.0));
    let snap: Vec<_> = reg.live_snapshot();
    assert_eq!(snap.len(), 2);
    let by_id: std::collections::HashMap<i64, _> = snap
        .into_iter()
        .map(|lv| (lv.tag_id, lv))
        .collect();
    assert_eq!(by_id[&1].value, Some(5.0));
    assert_eq!(by_id[&2].value, None);
}

#[tokio::test]
async fn tag_registry_collect_due_returns_pending_samples() {
    let reg = TagRegistry::new();
    let t1 = Tag::new(1, 1, "t1", "HR100", "FLOAT", 1.0, 0.0, "", "Общие", 1000, 0.0);
    reg.replace_all(&[t1]);
    reg.update_value(1, Some(10.0), QUALITY_GOOD, None);
    let due = reg.collect_due(now());
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].tag_id, 1);
    assert_eq!(due[0].value, 10.0);
    assert_eq!(due[0].quality, QUALITY_GOOD);
    assert!(reg.collect_due(now()).is_empty());
}

#[tokio::test]
async fn tag_registry_read_values_only_requested_tags() {
    let reg = TagRegistry::new();
    let t1 = Tag::new(1, 1, "t1", "HR100", "FLOAT", 1.0, 0.0, "", "Общие", 1000, 0.0);
    let t2 = Tag::new(2, 1, "t2", "HR101", "FLOAT", 1.0, 0.0, "", "Общие", 1000, 0.0);
    reg.replace_all(&[t1, t2]);
    reg.update_value(1, Some(1.0), QUALITY_GOOD, None);
    reg.update_value(2, Some(2.0), QUALITY_GOOD, None);
    let vals: Vec<_> = reg.read_values([2]);
    assert_eq!(vals.len(), 1);
    assert_eq!(vals[0].tag_id, 2);
    assert_eq!(vals[0].value, Some(2.0));
}

#[tokio::test]
async fn tag_point_pending_capacity_drops_oldest() {
    let tag = fresh_tag(1, 1);
    let tp = TagPoint::from_tag(&tag);
    let mut t = now();
    for i in 0..12i64 {
        tp.update(Some(i as f64), Some(i as f64), QUALITY_GOOD, t);
        t = t + Duration::milliseconds(10);
    }
    let samples = tp.drain_samples();
    assert_eq!(samples.len(), 8, "MAX_PENDING_SAMPLES = 8");
    assert_eq!(samples.first().unwrap().1, 4.0);
    assert_eq!(samples.last().unwrap().1, 11.0);
}
    let t1 = Tag::new(1, 10, "t1", "HR100", "FLOAT", 1.0, 0.0, "", "Общие", 1000, 0.0);
    let t2 = Tag::new(2, 10, "t2", "HR101", "FLOAT", 1.0, 0.0, "", "Общие", 1000, 0.0);
    let t3 = Tag::new(3, 20, "t3", "HR200", "FLOAT", 1.0, 0.0, "", "Общие", 1000, 0.0);
    reg.replace_all(&[t1, t2, t3]);
    reg.forget_connection(10);
    assert!(reg.get(1).is_none());
    assert!(reg.get(2).is_none());
    assert!(reg.get(3).is_some());
    assert_eq!(reg.len(), 1);
    assert_eq!(reg.points_for(10).len(), 0);
    assert_eq!(reg.points_for(20).len(), 1);
}
