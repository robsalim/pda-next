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
use crate::core::virtual_calc::{self, VirtualPlan};
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
                "экспоненциальное сглаживание сигнала (τ — постоянная времени, мс)",
            ),
        ],
        &[
            ("[12] - [13]", "FLOAT", "разность двух сигналов"),
            ("[40] * 0.1 + 20", "FLOAT", "масштабирование"),
            ("[12] > 100", "BOOL", "условие «перегрев»"),
            ("([1] > 80) && ([2] < 50)", "BOOL", "комбинация условий"),
            ("AVG([12], 5000)", "FLOAT", "плавно сглаженное значение (τ = 5 с)"),
            ("AVG([12], 60000) - [12]", "FLOAT", "отклонение от сглаженного за минуту"),
        ],
    )
}

/// Проверка выражения (для /api/validate/address virtual-подключений).
/// Максимум τ AVG проверяет парсер — здесь только синтаксис.
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

/// Состояние экспоненциального сглаживания AVG в живых вычислениях.
///
/// На каждую пару (сигнал, τ) хранится одно число — сглаженное значение и
/// момент последнего учтённого отсчёта. Продвижение только при новом
/// timestamp, поэтому дребезг/повторы одного опроса не сдвигают результат.
#[derive(Default)]
pub struct AvgBuffers {
    inner: std::collections::BTreeMap<(i64, i64), (i64, f64)>,
}

impl AvgBuffers {
    /// Учесть отсчёт тега (ts_ms, value) в сглаживании с постоянной τ.
    fn advance(&mut self, id: i64, tau_ms: f64, ts_ms: i64, value: f64) {
        virtual_calc::advance_ema(&mut self.inner, id, tau_ms, ts_ms, value);
    }

    /// Сглаженное значение (None — годных отсчётов ещё не было).
    fn get(&self, id: i64, tau_ms: f64) -> Option<f64> {
        self.inner
            .get(&(id, virtual_calc::tau_key(tau_ms)))
            .map(|(_, e)| *e)
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

    // Состояние AVG живёт между циклами (для AVG).
    let mut buffers = AvgBuffers::default();

    loop {
        if !state.running.load(Ordering::Relaxed) {
            break;
        }
        let started = std::time::Instant::now();
        let n = compute_once(&registry, &plan, Some(conn_cfg.id), &mut buffers);
        let cycle_s = started.elapsed().as_secs_f64();
        update_stats(&state, cycle_s, cycle_s, 0.0, 1, n);
        // Вычислителю не нужна связь с ПЛК, но цикл обязан крутиться:
        // зависшая задача — повод супервизору переподнять модуль.
        state.mark_ok();
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
/// `buffers` — состояние сглаживания AVG; продвигается отсчётами тегов,
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

    // Сглаживание AVG: продвигаем состояние свежими отсчётами операндов из
    // реестра (виртуалы-операнды — их значения с прошлого цикла; результат
    // текущего цикла попадёт в состояние на следующем префилле). Шаг α
    // считается по timestamp отсчёта, как при пересчёте истории.
    // plan.avg_needs заполняется в VirtualPlan::build.
    for &(id, tau) in &guard.avg_needs {
        if let Some(p) = registry.get(id) {
            let (ts, value, quality) = p.live_triple();
            if quality == QUALITY_GOOD {
                if let (Some(ts), Some(v)) = (ts, value) {
                    buffers.advance(id, tau, ts.timestamp_millis(), v);
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
            // Состояние AVG продвинуто в префилле отсчётами реестра.
            buffers.get(id, w)
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
        // AVG: синтаксис валиден, слишком большая τ — ошибка парсера.
        assert!(validate_expression("AVG([1], 5000)").is_none());
        assert!(validate_expression("AVG([1], 9999999)").unwrap().contains("AVG"));
    }

    #[test]
    fn avg_buffers_ema_math() {
        let mut b = AvgBuffers::default();
        // Отсчёты на t=0,10,20,30 (мс) со значениями 1,2,3,4 при τ = 1000 мс.
        let a = 1.0 - (-10.0f64 / 1000.0).exp();
        let mut e = 1.0;
        for (t, v) in [(0, 1.0), (10, 2.0), (20, 3.0), (30, 4.0)] {
            b.advance(5, 1000.0, t, v);
            if t > 0 {
                e += a * (v - e);
            }
            let got = b.get(5, 1000.0).unwrap();
            assert!((got - e).abs() < 1e-12, "t={t}: {got} != {e}");
        }
        // Тот же отсчёт уже учтён, старый ts — состояние не сдвигается.
        b.advance(5, 1000.0, 25, 99.0);
        b.advance(5, 1000.0, 30, 99.0);
        assert!((b.get(5, 1000.0).unwrap() - e).abs() < 1e-12);
        // Другая τ — собственное состояния; неизвестный тег — None.
        assert_eq!(b.get(5, 2000.0), None);
        assert_eq!(b.get(42, 1000.0), None);
    }

    #[tokio::test]
    async fn live_avg_smooths_operand_steps() {
        let registry = std::sync::Arc::new(TagRegistry::new());
        registry.replace_all(&[
            Tag { id: 10, connection_id: 1, ..Default::default() },
            Tag {
                id: 20,
                connection_id: 7,
                address_str: "AVG([10], 1000)".into(),
                ..Default::default()
            },
        ]);
        let plan = plan_for(&[(20, "AVG([10], 1000)")]);
        assert_eq!(plan.read().unwrap().avg_needs, vec![(10, 1000.0)]);

        // Три цикла с отсчётами раз в секунду при τ = 1 с: сглаженное значение
        // плавно тянется к сигналу, а не прыгает к последнему показанию.
        let mut buffers = AvgBuffers::default();
        let mut prev_ms: Option<i64> = None;
        let mut expect: Option<f64> = None;
        for (i, v) in [10.0, 20.0, 30.0].into_iter().enumerate() {
            let ts = Local::now() - chrono::Duration::seconds((2 - i) as i64);
            let ts_ms = ts.timestamp_millis();
            registry.update_value(10, Some(v), QUALITY_GOOD, Some(ts));
            compute_once(&registry, &plan, None, &mut buffers);
            expect = Some(match (expect, prev_ms) {
                (Some(e), Some(p)) => {
                    let a = 1.0 - (-(ts_ms - p) as f64 / 1000.0).exp();
                    e + a * (v - e)
                }
                _ => v,
            });
            prev_ms = Some(ts_ms);
        }
        let got = registry.get(20).unwrap().live_triple().1.unwrap();
        assert!((got - expect.unwrap()).abs() < 1e-9, "{got}");
        // 10 → 20 → 30: значение строго между крайними отсчётами (плавно).
        assert!(got > 10.0 && got < 30.0, "{got}");
    }

    #[tokio::test]
    async fn avg_operand_missing_gives_bad() {
        // Тег без значений вообще — состояния сглаживания нет, результат недоступен.
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
