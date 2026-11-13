//! Драйвер виртуальных сигналов (driver_type = 'virtual').
//!
//! Не опрашивает оборудование: после каждого цикла берёт текущие значения
//! реальных тегов из реестра, вычисляет выражения виртуалов в топологическом
//! порядке и публикует результат обратно в реестр. Виртуал — обычный TagPoint,
//! поэтому живые таблицы, WebSocket, графики (включая битовые дорожки для
//! BOOL) работают с ним без специальных веток.
//!
//! В историю виртуалы НЕ пишутся (см. historian-фильтр exclude_ids): их
//! прошлое вычисляется на лету из истории операндов в /api/series.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use chrono::Local;

use crate::core::tag_point::{QUALITY_BAD, QUALITY_GOOD};
use crate::core::tag_registry::TagRegistry;
use crate::core::virtual_calc::VirtualPlan;
use crate::core::virtual_expr;
use crate::models::Connection;

use super::base::{update_stats, DriverState};

/// Период пересчёта, когда у подключения не задан осмысленный интервал.
const DEFAULT_PERIOD_MS: u64 = 500;

/// Подпись синтаксиса для конструктора выражений в интерфейсе.
pub fn address_info() -> serde_json::Value {
    super::base::address_json(
        "Выражение из [id] других сигналов: [12] - [13], [40] * 0.1 + 20, AVG([12], 5000)",
        &[
            ("[id]", "значение сигнала по его id"),
            ("+ - * / ( )", "арифметика"),
            ("< <= > >= == !=", "сравнения (результат 1/0)"),
            ("&& || !", "логика"),
            (
                "AVG([id], ms)",
                "скользящее среднее сигнала за окно миллисекунд",
            ),
        ],
        &[
            ("[12] - [13]", "FLOAT", "разность двух сигналов"),
            ("[40] * 0.1 + 20", "FLOAT", "масштабирование"),
            ("[12] > 100", "BOOL", "условие «перегрев»"),
            ("([1] > 80) && ([2] < 50)", "BOOL", "комбинация условий"),
            ("AVG([12], 5000)", "FLOAT", "среднее за 5 секунд"),
            ("AVG([12], 60000) - [12]", "FLOAT", "отклонение от минуты среднего"),
        ],
    )
}

/// Проверка выражения (для /api/validate/address virtual-подключений).
/// Максимум окна AVG проверяет парсер — здесь только синтаксис.
pub fn validate_expression(src: &str) -> Option<String> {
    match virtual_expr::parse(src) {
        Ok(_) => None,
        Err(e) => {
            let at = if e.pos == usize::MAX {
                String::new()
            } else {
                format!(" (позиция {})", e.pos + 1)
            };
            Some(format!("{}{at}", e.message))
        }
    }
}

/// Скользящие буферы отсчётов для AVG в живых вычислениях.
///
/// Для каждого тега, встречающегося внутри AVG, храним последние отсчёты
/// (время мс, значение) не старше максимального окна. Обновление — только
/// при новом timestamp, поэтому дребезг/повторы одного опроса не удваивают
/// точки буфера.
#[derive(Default)]
pub struct AvgBuffers {
    inner: HashMap<i64, std::collections::VecDeque<(i64, f64)>>,
}

impl AvgBuffers {
    /// Страховка от разрастания при больших окнах и частом опросе:
    /// лишние старейшие отсчёты вытесняются.
    const MAX_SAMPLES: usize = 20_000;

    /// Зарегистрировать отсчёт тега, если он свежее последнего.
    fn push(&mut self, id: i64, ts_ms: i64, value: f64, cap_ms: f64) {
        let dq = self.inner.entry(id).or_default();
        if let Some(last) = dq.back_mut() {
            if last.0 == ts_ms {
                // Тот же такт опроса — последнее показание выигрывает.
                last.1 = value;
                return;
            }
            if last.0 > ts_ms {
                return;
            }
        }
        dq.push_back((ts_ms, value));
        // Обрезаем старьё за пределами максимального окна этого тега.
        let floor = ts_ms - cap_ms.ceil() as i64;
        while dq.front().map_or(false, |&(t, _)| t < floor) {
            dq.pop_front();
        }
        while dq.len() > Self::MAX_SAMPLES {
            dq.pop_front();
        }
    }

    /// Среднее по окну (now_ms - window_ms, now_ms]. Пусто — None.
    fn mean(&self, id: i64, now_ms: i64, window_ms: f64) -> Option<f64> {
        let dq = self.inner.get(&id)?;
        let floor = now_ms - window_ms.ceil() as i64;
        let mut sum = 0.0;
        let mut n = 0usize;
        for &(t, v) in dq.iter().rev() {
            if t <= floor {
                break;
            }
            if t > now_ms {
                continue; // часы устройства спешат — будущий отсчёт не в окне
            }
            sum += v;
            n += 1;
        }
        if n == 0 {
            None
        } else {
            Some(sum / n as f64)
        }
    }
}

/// Цикл вычислителя. `plan` обновляется менеджером при reload конфигурации.
/// Задача считает только виртуалы своего подключения (общий план общий для
/// всех virtual-модулей: операнды могут быть из другого модуля).
pub async fn run_virtual(
    conn_cfg: std::sync::Arc<Connection>,
    registry: std::sync::Arc<TagRegistry>,
    state: std::sync::Arc<DriverState>,
    plan: std::sync::Arc<std::sync::RwLock<VirtualPlan>>,
) {
    let period_ms = if conn_cfg.poll_interval_ms > 0 {
        conn_cfg.poll_interval_ms.max(20) as u64
    } else {
        DEFAULT_PERIOD_MS
    };
    state.running.store(true, Ordering::Relaxed);
    state.set_status("Calculating");

    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(period_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Скользящие буферы живут между циклами (для AVG).
    let mut buffers = AvgBuffers::default();

    loop {
        if !state.running.load(Ordering::Relaxed) {
            break;
        }
        let started = std::time::Instant::now();
        let n = compute_once(&registry, &plan, Some(conn_cfg.id), &mut buffers);
        let cycle_s = started.elapsed().as_secs_f64();
        update_stats(&state, cycle_s, cycle_s, 0.0, 1, n);
        ticker.tick().await;
    }
    state.set_status("Stopped");
}

/// Один проход вычислений; `only_conn` — считать виртуалы одного подключения
/// (None — весь план). Возвращает число вычисленных виртуалов.
///
/// Публикация идёт через тот же registry.update_value(), что и драйверы
/// опроса: scale/offset применяются реестром, а value=None в выражении
/// операнды уже масштабированные значения (как их видит оператор).
/// `buffers` — скользящие окна для AVG; заполняются отсчётами тегов,
/// зарегистрированных в плане как операнды AVG.
pub fn compute_once(
    registry: &TagRegistry,
    plan: &std::sync::RwLock<VirtualPlan>,
    only_conn: Option<i64>,
    buffers: &mut AvgBuffers,
) -> usize {
    // Снимок плана короткой read-блокировкой: reload_tags может пересобрать
    // его прямо во время опроса.
    let guard = match plan.read() {
        Ok(g) => g,
        Err(_) => return 0,
    };
    // Значения, вычисленные в этом проходе, доступны последующим виртуалам
    // цепочки (deps уже упорядочены топосортировкой).
    let mut computed: HashMap<i64, (f64, i32)> = HashMap::new();
    let mut done = 0usize;

    let now = Local::now();
    let now_ms = now.timestamp_millis();

    // Окна AVG: наполняем буферы последними известными отсчётами операндов
    // из реестра (виртуалы-операнды — их значения с прошлого цикла;
    // результат текущего цикла попадёт в буфер на следующем префилле).
    // plan.avg_needs заполняется в VirtualPlan::build.
    if !guard.avg_needs.is_empty() {
        let ids: Vec<i64> = guard.avg_needs.keys().copied().collect();
        for id in ids {
            let cap = guard.avg_needs[&id];
            if let Some(p) = registry.get(id) {
                let (ts, value, quality) = p.live_triple();
                if quality == QUALITY_GOOD {
                    if let (Some(ts), Some(v)) = (ts, value) {
                        buffers.push(id, ts.timestamp_millis(), v, cap);
                    }
                }
            }
        }
    }

    for item in &guard.order {
        if only_conn.map_or(false, |c| item.conn_id != c) {
            continue;
        }
        let mut lookup = |id: i64| -> Option<f64> {
            if let Some((v, q)) = computed.get(&id) {
                return if *q == QUALITY_GOOD { Some(*v) } else { None };
            }
            let p = registry.get(id)?;
            let (_ts, value, quality) = p.live_triple();
            if quality != QUALITY_GOOD {
                return None;
            }
            value
        };
        let mut window = |id: i64, w: f64| -> Option<f64> {
            // Буфер AVG операнда наполнен в префилле (значения реестра на
            // момент прошлого цикла) — среднее берётся только из него.
            buffers.mean(id, now_ms, w)
        };
        let mut cx = virtual_expr::Context {
            lookup: &mut lookup,
            avg: Some(&mut window),
            now_ms,
        };
        match virtual_expr::eval_ctx(&item.node, &mut cx) {
            Some(v) => {
                computed.insert(item.tag_id, (v, QUALITY_GOOD));
                done += 1;
                // raw = значение до scale: реестр сам применит scale/offset,
                // поэтому отдаём сырой результат выражения.
                registry.update_value(item.tag_id, Some(v), QUALITY_GOOD, Some(now));
            }
            None => {
                // Один из операндов недоступен (BAD/нет данных) — виртуал
                // не может быть хорошим. Значение сохраняется прежним.
                computed.insert(item.tag_id, (f64::NAN, QUALITY_BAD));
                registry.mark_bad([item.tag_id]);
            }
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Tag;

    fn plan_for(exprs: &[(i64, &str)]) -> std::sync::Arc<std::sync::RwLock<VirtualPlan>> {
        let tags: Vec<Tag> = exprs
            .iter()
            .map(|(id, e)| Tag {
                id: *id,
                connection_id: 7,
                address_str: e.to_string(),
                ..Default::default()
            })
            .collect();
        std::sync::Arc::new(std::sync::RwLock::new(VirtualPlan::build(&tags)))
    }

    /// compute_once со своим буфером (для одноцикловых тестов).
    fn once(
        registry: &TagRegistry,
        plan: &std::sync::RwLock<VirtualPlan>,
    ) -> usize {
        let mut b = AvgBuffers::default();
        compute_once(registry, plan, None, &mut b)
    }

    #[tokio::test]
    async fn computes_chain_and_publishes() {
        let registry = std::sync::Arc::new(TagRegistry::new());
        // Реальные 10 и 11, виртуалы: 20 = [10]*2, 21 = [20] + [11]
        registry.replace_all(&[
            Tag { id: 10, connection_id: 1, ..Default::default() },
            Tag { id: 11, connection_id: 1, ..Default::default() },
            Tag { id: 20, connection_id: 7, address_str: "[10] * 2".into(), ..Default::default() },
            Tag { id: 21, connection_id: 7, address_str: "[20] + [11]".into(), ..Default::default() },
        ]);
        let now = Local::now();
        registry.update_value(10, Some(5.0), QUALITY_GOOD, Some(now));
        registry.update_value(11, Some(1.0), QUALITY_GOOD, Some(now));

        let plan = plan_for(&[(20, "[10] * 2"), (21, "[20] + [11]")]);
        once(&registry, &plan);

        assert_eq!(registry.get(20).unwrap().live_triple().1, Some(10.0));
        assert_eq!(registry.get(21).unwrap().live_triple().1, Some(11.0));
        assert_eq!(registry.get(21).unwrap().live_triple().2, QUALITY_GOOD);
    }

    #[tokio::test]
    async fn bad_operand_marks_virtual_bad() {
        let registry = std::sync::Arc::new(TagRegistry::new());
        registry.replace_all(&[
            Tag { id: 10, connection_id: 1, ..Default::default() },
            Tag { id: 20, connection_id: 7, address_str: "[10] * 2".into(), ..Default::default() },
        ]);
        let now = Local::now();
        registry.update_value(10, Some(5.0), QUALITY_GOOD, Some(now));

        let plan = plan_for(&[(20, "[10] * 2")]);
        once(&registry, &plan);
        assert_eq!(registry.get(20).unwrap().live_triple().2, QUALITY_GOOD);

        // Операнд потерялся -> виртуал становится BAD
        registry.update_value(10, Some(5.0), QUALITY_BAD, Some(now));
        once(&registry, &plan);
        assert_eq!(registry.get(20).unwrap().live_triple().2, QUALITY_BAD);
    }

    #[tokio::test]
    async fn bool_comparison_yields_one_or_zero() {
        let registry = std::sync::Arc::new(TagRegistry::new());
        registry.replace_all(&[
            Tag { id: 10, connection_id: 1, data_type: "FLOAT".into(), ..Default::default() },
            Tag { id: 20, connection_id: 7, data_type: "BOOL".into(), address_str: "[10] > 100".into(), ..Default::default() },
        ]);
        let now = Local::now();
        registry.update_value(10, Some(150.0), QUALITY_GOOD, Some(now));
        let plan = plan_for(&[(20, "[10] > 100")]);
        once(&registry, &plan);
        assert_eq!(registry.get(20).unwrap().live_triple().1, Some(1.0));
        registry.update_value(10, Some(50.0), QUALITY_GOOD, Some(now));
        once(&registry, &plan);
        assert_eq!(registry.get(20).unwrap().live_triple().1, Some(0.0));
    }

    #[tokio::test]
    async fn error_expression_leaves_point_untouched() {
        // Планировщик отсекает битое выражение — в order его нет, точка не меняется.
        let registry = std::sync::Arc::new(TagRegistry::new());
        registry.replace_all(&[Tag {
            id: 20,
            connection_id: 7,
            address_str: "[10] +".into(),
            ..Default::default()
        }]);
        let plan = plan_for(&[(20, "[10] +")]);
        assert!(plan.read().unwrap().errors.contains_key(&20));
        assert_eq!(once(&registry, &plan), 0);
        assert_eq!(registry.get(20).unwrap().live_triple().1, None);
    }

    #[test]
    fn validate_reports_position() {
        assert!(validate_expression("[1] + [2]").is_none());
        // Ошибка не в конце строки -> должна содержать позицию.
        let e = validate_expression("[1] + )").unwrap();
        assert!(e.contains("позиция 7"), "{e}");
        // Обрыв на конце -> без позиции, но с текстом ошибки.
        let e2 = validate_expression("[1] +").unwrap();
        assert!(!e2.is_empty());
        // AVG: синтаксис валиден, слишком большое окно — ошибка парсера.
        assert!(validate_expression("AVG([1], 5000)").is_none());
        assert!(validate_expression("AVG([1], 9999999)").unwrap().contains("AVG"));
    }

    #[test]
    fn avg_buffers_window_math() {
        let mut b = AvgBuffers::default();
        // Отсчёты на t=0,10,20,30 (мс) со значениями 1,2,3,4
        for (t, v) in [(0, 1.0), (10, 2.0), (20, 3.0), (30, 4.0)] {
            b.push(5, t, v, 1000.0);
        }
        // Повтор/старый ts игнорируются
        b.push(5, 25, 99.0, 1000.0);
        assert_eq!(b.mean(5, 30, 1000.0), Some(2.5)); // (1+2+3+4)/4
        assert_eq!(b.mean(5, 30, 20.0), Some(3.5)); // (10,30] -> {3,4}
        assert_eq!(b.mean(5, 30, 11.0), Some(3.5)); // (19,30] -> {3,4}
        assert_eq!(b.mean(5, 30, 5.0), Some(4.0)); // (25,30] -> {4}
        assert_eq!(b.mean(5, 30, 100_000.0), Some(2.5)); // всё окно
        assert_eq!(b.mean(42, 30, 100.0), None); // нет такого тега
    }

    #[tokio::test]
    async fn live_avg_over_recent_cycles() {
        let registry = std::sync::Arc::new(TagRegistry::new());
        registry.replace_all(&[
            Tag { id: 10, connection_id: 1, ..Default::default() },
            Tag {
                id: 20,
                connection_id: 7,
                address_str: "AVG([10], 500000)".into(),
                ..Default::default()
            },
        ]);
        let plan = plan_for(&[(20, "AVG([10], 500000)")]);
        assert_eq!(plan.read().unwrap().avg_needs.get(&10), Some(&500_000.0));

        // Три цикла вычислений с разнесёнными метками: каждое новое показание
        // попадает в буфер, среднее по большому окну = среднее всех показаний.
        let mut buffers = AvgBuffers::default();
        for (i, v) in [10.0, 20.0, 30.0].into_iter().enumerate() {
            let ts = Local::now() - chrono::Duration::seconds((2 - i) as i64);
            registry.update_value(10, Some(v), QUALITY_GOOD, Some(ts));
            compute_once(&registry, &plan, None, &mut buffers);
        }
        assert_eq!(registry.get(20).unwrap().live_triple().1, Some(20.0));
    }

    #[tokio::test]
    async fn avg_operand_missing_gives_bad() {
        // Тег без значений вообще — окно пустое, результат недоступен.
        let registry = std::sync::Arc::new(TagRegistry::new());
        registry.replace_all(&[
            Tag { id: 10, connection_id: 1, ..Default::default() },
            Tag { id: 20, connection_id: 7, address_str: "AVG([10], 5000)".into(), ..Default::default() },
        ]);
        let plan = plan_for(&[(20, "AVG([10], 5000)")]);
        assert_eq!(compute_once(&registry, &plan, None, &mut AvgBuffers::default()), 0);
        assert_eq!(registry.get(20).unwrap().live_triple().2, QUALITY_BAD);
    }
}
