//! План вычислений виртуальных сигналов.
//!
//! Виртуальный тег — тег подключения driver_type='virtual', в address_str
//! которого лежит выражение из [id] других сигналов (virtual_expr). План
//! компилируется при каждой загрузке конфигурации:
//!   * разбираются выражения (синтаксические ошибки не роняют остальные);
//!   * виртуалы, зависящие друг от друга, упорядочиваются топологически;
//!   * циклические зависимости выявляются и отбраковываются с текстом ошибки.
//!
//! Значения публикацией в реестр и качеством занимается задача-вычислитель
//! (drivers/virtual_calc.rs), история виртуалов пересчитывается из истории
//! операндов на лету (pda-server/src/api.rs) — в БД виртуалы не пишутся.

use std::collections::{HashMap, HashSet};

use super::super::models::Tag;
use super::virtual_expr::{self, Node};

/// Один скомпилированный виртуальный сигнал.
pub struct VirtualItem {
    pub tag_id: i64,
    /// Подключение, которому принадлежит виртуал (фильтр задачи-вычислителя).
    pub conn_id: i64,
    pub node: Node,
    /// Виртуальные операнды (внешние операнды читаются как есть).
    pub deps: Vec<i64>,
}

/// Скомпилированный план: порядок вычислений + ошибки компиляции.
#[derive(Default)]
pub struct VirtualPlan {
    /// Виртуалы в топологическом порядке (зависимые идут после операндов).
    pub order: Vec<VirtualItem>,
    /// tag_id -> текст ошибки (битое/циклическое выражение).
    pub errors: HashMap<i64, String>,
    /// Сигналы, используемые внутри AVG: tag_id -> максимальное окно, мс.
    /// Живой вычислитель ведёт скользящие буферы только для этих тегов.
    pub avg_needs: HashMap<i64, f64>,
}

impl VirtualPlan {
    /// Компиляция из тегов, принадлежащих virtual-подключениям.
    pub fn build(virtual_tags: &[Tag]) -> Self {
        let mut plan = Self::default();
        let ids: HashSet<i64> = virtual_tags.iter().map(|t| t.id).collect();

        struct Candidate {
            tag_id: i64,
            conn_id: i64,
            node: Node,
            /// Виртуальные операнды, от которых нужно дождаться значения.
            virt_deps: Vec<i64>,
        }
        let mut candidates: Vec<Candidate> = Vec::new();

        for t in virtual_tags {
            let node = match virtual_expr::parse(&t.address_str) {
                Ok(n) => n,
                Err(e) => {
                    // pos == usize::MAX означает «конец строки» — не инкрементируем
                    let at = if e.pos == usize::MAX {
                        e.message.clone()
                    } else {
                        format!("{} (позиция {})", e.message, e.pos + 1)
                    };
                    plan.errors.insert(t.id, at);
                    continue;
                }
            };
            let mut all = HashSet::new();
            virtual_expr::dependencies(&node, &mut all);
            if all.contains(&t.id) {
                plan.errors
                    .insert(t.id, "выражение ссылается на себя".into());
                continue;
            }
            let virt_deps: Vec<i64> = all.into_iter().filter(|i| ids.contains(i)).collect();
            candidates.push(Candidate {
                tag_id: t.id,
                conn_id: t.connection_id,
                node,
                virt_deps,
            });
        }

        // Топосортировка (Каан через повторные проходы: виртуалов всегда
        // десятки, квадратичный проход дешевле связного графа).
        let mut done: HashSet<i64> = HashSet::new();
        loop {
            let mut progressed = false;
            for c in &mut candidates {
                if done.contains(&c.tag_id) {
                    continue;
                }
                if c.virt_deps.iter().all(|d| done.contains(d)) {
                    let deps = std::mem::take(&mut c.virt_deps);
                    plan.order.push(VirtualItem {
                        tag_id: c.tag_id,
                        conn_id: c.conn_id,
                        node: c.node.clone(),
                        deps,
                    });
                    done.insert(c.tag_id);
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }
        for c in &candidates {
            if !done.contains(&c.tag_id) {
                plan.errors
                    .insert(c.tag_id, "циклическая зависимость виртуалов".into());
            }
        }

        // Окна AVG: буферизуем все операнды функции с запасом по максимальному
        // окну (виртуалы-операнды — их собственные результаты).
        for item in &plan.order {
            let mut calls = Vec::new();
            virtual_expr::avg_calls(&item.node, &mut calls);
            for (id, w) in calls {
                let e = plan.avg_needs.entry(id).or_insert(0.0);
                if w > *e {
                    *e = w;
                }
            }
        }
        plan
    }

    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

// ------------------------------------------------------- история виртуалов
//
// Виртуалы не пишутся в БД; их прошлое вычисляется на запрос из истории
// операндов. Семантика совпадает с densify_buckets для реальных тегов:
// step-hold (последнее известное значение <= t), NaN — пустой бакет
// прореженной сетки —hold-ится (не сбрасывает удержанное значение); NaN в
// результате только до первой доступной точки выражения — uPlot рисует разрыв.

use std::collections::BTreeMap;

/// Пересчёт историй виртуальных тегов из историй операндов.
///
/// * `tag_info(id)` — Some((is_virtual, address_src)) по id тега (None, если
///   тега нет в конфигурации);
/// * `real_series` — истории реальных тегов: ts_миллисекунды (возрастание) +
///   значения (могут содержать NaN как «нет данных»);
/// * возвращает истории запрошенных виртуалов; вложенные виртуалы-операнды
///   вычисляются рекурсивно, циклы дают пустой результат.
pub fn eval_history(
    virt_ids: &[i64],
    tag_info: &impl Fn(i64) -> Option<(bool, String)>,
    real_series: &BTreeMap<i64, Vec<(i64, f64)>>,
) -> BTreeMap<i64, Vec<(i64, f64)>> {
    let mut memo: BTreeMap<i64, Vec<(i64, f64)>> = BTreeMap::new();
    let mut visiting: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for id in virt_ids {
        let _ = history_one(*id, tag_info, real_series, &mut memo, &mut visiting);
    }
    memo
}

fn history_one(
    id: i64,
    tag_info: &impl Fn(i64) -> Option<(bool, String)>,
    real_series: &BTreeMap<i64, Vec<(i64, f64)>>,
    memo: &mut BTreeMap<i64, Vec<(i64, f64)>>,
    visiting: &mut std::collections::HashSet<i64>,
) -> Option<Vec<(i64, f64)>> {
    if let Some(v) = memo.get(&id) {
        return Some(v.clone());
    }
    if real_series.contains_key(&id) {
        return real_series.get(&id).cloned();
    }
    let (is_virtual, src) = tag_info(id)?;
    if !is_virtual {
        // Реальный тег без данных в окне — пустая серия (в real_series её нет).
        return Some(Vec::new());
    }
    if !visiting.insert(id) {
        return None; // цикл — считаем недоступный операнд
    }
    let node = match virtual_expr::parse(&src) {
        Ok(n) => n,
        Err(_) => {
            visiting.remove(&id);
            memo.insert(id, Vec::new());
            return Some(Vec::new());
        }
    };
    let mut refs = std::collections::HashSet::new();
    virtual_expr::dependencies(&node, &mut refs);

    // Истории всех операндов в одном виде (рекурсивно для вложенных виртуалов).
    let mut operand: BTreeMap<i64, Vec<(i64, f64)>> = BTreeMap::new();
    for r in refs {
        match history_one(r, tag_info, real_series, memo, visiting) {
            Some(s) => {
                operand.insert(r, s);
            }
            // Операнда нет в конфигурации (или циклическая ссылка) — весь
            // виртуал недоступен.
            None => {
                visiting.remove(&id);
                memo.insert(id, Vec::new());
                return Some(Vec::new());
            }
        }
    }
    visiting.remove(&id);
    let out = eval_on_union(&node, &operand);
    memo.insert(id, out.clone());
    Some(out)
}

/// Вычисление на объединённой сетке времени операндов.
/// Семантика как у densify_buckets: вне «занятого» диапазона операнда
/// (первые/последние NaN-бакеты) — значения нет, дыры внутри диапазона
/// удерживают предыдущее известное значение (step-hold).
fn eval_on_union(node: &Node, operand: &BTreeMap<i64, Vec<(i64, f64)>>) -> Vec<(i64, f64)> {
    let mut grid: Vec<i64> = operand
        .values()
        .flat_map(|s| s.iter().map(|(t, _)| *t))
        .collect();
    grid.sort_unstable();
    grid.dedup();
    if grid.is_empty() {
        return Vec::new();
    }
    struct Cursor<'a> {
        id: &'a i64,
        series: &'a [(i64, f64)],
        pos: usize,
        last: Option<f64>,
        /// Границы занятых данных (как first_busy/last_busy в densify_buckets).
        busy_from: i64,
        busy_to: i64,
    }
    let mut cursors: Vec<Cursor> = Vec::new();
    for (id, s) in operand {
        let first = s.iter().find(|(_, v)| !v.is_nan()).map(|(t, _)| *t);
        let last_busy = s
            .iter()
            .rev()
            .find(|(_, v)| !v.is_nan())
            .map(|(t, _)| *t);
        match (first, last_busy) {
            (Some(f), Some(l)) => cursors.push(Cursor {
                id,
                series: s,
                pos: 0,
                last: None,
                busy_from: f,
                busy_to: l,
            }),
            // Операнд без данных в окне -> весь результат пуст.
            _ => return Vec::new(),
        }
    }
    let mut out = Vec::with_capacity(grid.len());
    // Префиксные суммы незанулённых (не-NaN) отсчётов для AVG по истории.
    // NaN (пустые бакеты прореженной сетки) в среднее не входят.
    let mut avg_data: BTreeMap<i64, (Vec<i64>, Vec<f64>)> = BTreeMap::new();
    for (id, s) in operand {
        let mut ts = Vec::with_capacity(s.len());
        let mut pre = Vec::with_capacity(s.len());
        let mut sum = 0.0f64;
        for (t, v) in s {
            if !v.is_nan() {
                ts.push(*t);
                sum += v;
                pre.push(sum);
            }
        }
        avg_data.insert(*id, (ts, pre));
    }
    let win_avg = |id: i64, now_ms: i64, w: f64| -> Option<f64> {
        let (ts, pre) = avg_data.get(&id)?;
        if ts.is_empty() {
            return None;
        }
        // Отсчёты в (now_ms - w, now_ms]
        let floor = now_ms - w.ceil() as i64;
        let hi = ts.partition_point(|t| *t <= now_ms);
        let lo = ts.partition_point(|t| *t <= floor);
        if hi <= lo {
            return None;
        }
        let sum = pre[hi - 1] - if lo > 0 { pre[lo - 1] } else { 0.0 };
        Some(sum / (hi - lo) as f64)
    };
    for t in grid {
        for c in cursors.iter_mut() {
            while c.pos < c.series.len() && c.series[c.pos].0 <= t {
                let v = c.series[c.pos].1;
                if !v.is_nan() {
                    c.last = Some(v);
                }
                c.pos += 1;
            }
        }
        let mut window = |id: i64, w: f64| win_avg(id, t, w);
        let mut cx = virtual_expr::Context {
            lookup: &mut |id| {
                cursors
                    .iter()
                    .find(|c| *c.id == id)
                    .filter(|c| t >= c.busy_from && t <= c.busy_to)
                    .and_then(|c| c.last)
            },
            avg: Some(&mut window),
            now_ms: t,
        };
        let v = virtual_expr::eval_ctx(node, &mut cx);
        out.push((t, v.unwrap_or(f64::NAN)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(id: i64, expr: &str) -> Tag {
        Tag {
            id,
            connection_id: 9,
            address_str: expr.into(),
            ..Default::default()
        }
    }

    #[test]
    fn topological_order_respects_dependencies() {
        // v2 = v1 * 2, v1 = [10] + 1 => v1 должен считаться раньше
        let plan = VirtualPlan::build(&[tag(2, "[1] * 2"), tag(1, "[10] + 1")]);
        assert!(plan.is_ok());
        assert_eq!(plan.order[0].tag_id, 1);
        assert_eq!(plan.order[1].tag_id, 2);
    }

    #[test]
    fn cycle_detected() {
        let plan = VirtualPlan::build(&[tag(1, "[2] + 1"), tag(2, "[1] + 1")]);
        assert!(plan.order.is_empty());
        assert_eq!(plan.errors.len(), 2);
        for msg in plan.errors.values() {
            assert!(msg.contains("цикл"), "{msg}");
        }
    }

    #[test]
    fn self_reference_rejected() {
        let plan = VirtualPlan::build(&[tag(1, "[1] * 2")]);
        assert!(plan.errors.contains_key(&1));
        assert_eq!(plan.order.len(), 0);
    }

    #[test]
    fn syntax_error_isolated() {
        let plan = VirtualPlan::build(&[tag(1, "[10] +"), tag(2, "[10] * 2")]);
        assert!(plan.errors.contains_key(&1));
        assert_eq!(plan.order.len(), 1);
        assert_eq!(plan.order[0].tag_id, 2);
    }

    #[test]
    fn chain_of_three() {
        let plan = VirtualPlan::build(&[tag(3, "[2] + 1"), tag(2, "[1] + 1"), tag(1, "[10]")]);
        let order: Vec<i64> = plan.order.iter().map(|i| i.tag_id).collect();
        assert_eq!(order, vec![1, 2, 3]);
    }

    // ------------------------------------------------------------ история
    fn hist(cases: &[(i64, &str)], real: Vec<(i64, Vec<(i64, f64)>)>) -> Vec<(i64, f64)> {
        let tags: std::collections::HashMap<i64, (bool, String)> = cases
            .iter()
            .map(|(id, e)| (*id, (true, e.to_string())))
            .collect();
        let real_map: BTreeMap<i64, Vec<(i64, f64)>> = real.into_iter().collect();
        let info = |id: i64| -> Option<(bool, String)> {
            tags.get(&id).cloned().or_else(|| {
                // Неизвестные id — реальные теги
                Some((false, String::new()))
            })
        };
        let virt_ids: Vec<i64> = cases.iter().map(|(id, _)| *id).collect();
        eval_history(&virt_ids, &info, &real_map)
            .get(&cases.last().unwrap().0)
            .cloned()
            .unwrap_or_default()
    }

    #[test]
    fn history_arithmetic_step_hold() {
        // Операнды с разными сетками: шаг-холд по объединённым временам,
        // значения вне «занятого» диапазона операнда отсутствуют (как в
        // densify_buckets реальных прореженных серий).
        let out = hist(
            &[(1, "[10] + [11]")],
            vec![
                (10, vec![(0, 1.0), (10, 2.0)]),
                (11, vec![(5, 10.0), (10, 10.0)]),
            ],
        );
        // t=0: [11] ещё не началось -> NaN; t=5: 1+10; t=10: 2+10
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].0, 0);
        assert!(out[0].1.is_nan());
        assert_eq!(out[1], (5, 11.0));
        assert_eq!(out[2], (10, 12.0));
    }

    #[test]
    fn history_nan_hole_holds_previous() {
        // densify_buckets: NaN внутри занятого диапазона — дыра сетки,
        // step-hold держит предыдущее; NaN до/после границ данных — нет значения.
        let out = hist(
            &[(1, "[10] * 2")],
            vec![(10, vec![(0, f64::NAN), (10, 1.0), (20, f64::NAN), (30, 3.0)])],
        );
        assert!(out[0].1.is_nan()); // до busy_from
        assert_eq!(out[1].1, 2.0);
        assert_eq!(out[2].1, 2.0); // step-hold через дыру
        assert_eq!(out[3].1, 6.0);
    }

    #[test]
    fn history_nested_virtuals() {
        // v1 = [10]*2 ; v2 = v1 + 1
        let tags: std::collections::HashMap<i64, (bool, String)> =
            [(1, (true, "[10] * 2".to_string())), (2, (true, "[1] + 1".to_string()))]
                .into_iter()
                .collect();
        let info = |id: i64| tags.get(&id).cloned().or(Some((false, String::new())));
        let real: BTreeMap<i64, Vec<(i64, f64)>> =
            [(10i64, vec![(0i64, 5.0), (10, 6.0)])].into_iter().collect();
        let res = eval_history(&[2], &info, &real);
        assert_eq!(res.get(&1).unwrap(), &vec![(0, 10.0), (10, 12.0)]);
        assert_eq!(res.get(&2).unwrap(), &vec![(0, 11.0), (10, 13.0)]);
    }

    #[test]
    fn history_cycle_gives_empty() {
        let tags: std::collections::HashMap<i64, (bool, String)> =
            [(1, (true, "[2]".to_string())), (2, (true, "[1]".to_string()))]
                .into_iter()
                .collect();
        let info = |id: i64| tags.get(&id).cloned();
        let res = eval_history(&[1], &info, &BTreeMap::new());
        assert!(res.get(&1).map(|v| v.is_empty()).unwrap_or(false));
    }

    #[test]
    fn history_comparison_bool() {
        let out = hist(
            &[(1, "[10] > 100")],
            vec![(10, vec![(0, 50.0), (10, 150.0)])],
        );
        assert_eq!(out, vec![(0, 0.0), (10, 1.0)]);
    }

    #[test]
    fn history_avg_over_operand() {
        // AVG([10], 20): окно (t-20, t], NaN-дыры не участвуют в среднем.
        let out = hist(
            &[(1, "AVG([10], 20)")],
            vec![(10, vec![(0, 1.0), (10, 3.0), (20, f64::NAN), (30, 5.0)])],
        );
        // t=0: окно (-20,0] -> {1.0} => 1.0
        // t=10: (-10,10] -> {1,3} => 2.0
        // t=20: (0,20] -> {3} (NaN не входит, 1.0 на floor исключена) => 3.0
        // t=30: (10,30] -> {5} (3.0 на t=10 = floor исключена) => 5.0
        assert_eq!(out[0].1, 1.0);
        assert_eq!(out[1].1, 2.0);
        assert_eq!(out[2].1, 3.0);
        assert_eq!(out[3].1, 5.0);
    }

    #[test]
    fn history_avg_mixed_with_ref() {
        let out = hist(
            &[(1, "[10] - AVG([10], 15)")],
            vec![(10, vec![(0, 10.0), (10, 20.0), (20, 30.0)])],
        );
        // t=0: 10 - 10 = 0; t=10: 20 - avg(10,20)=15 => 5; t=20: 30 - 25 = 5
        // (окно 15: на t=20 в (5,20] входят точки 10 и 20 => среднее 25)
        assert_eq!(out[0].1, 0.0);
        assert_eq!(out[1].1, 5.0);
        assert_eq!(out[2].1, 5.0);
    }

    #[test]
    fn plan_collects_avg_needs() {
        // v1 = AVG([10], 5000): буфер нужен для реального 10 с окном 5000.
        let plan = VirtualPlan::build(&[tag(1, "AVG([10], 5000)")]);
        assert!(plan.is_ok());
        assert_eq!(plan.avg_needs.get(&10), Some(&5000.0));
        // окно берётся максимумом по всем вызовам одного тега
        let plan = VirtualPlan::build(&[tag(1, "AVG([10], 5000) + AVG([10], 20000)")]);
        assert_eq!(plan.avg_needs.get(&10), Some(&20000.0));
    }
}
