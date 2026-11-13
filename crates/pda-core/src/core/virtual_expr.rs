//! Парсер и вычислитель выражений виртуальных сигналов.
//!
//! Выражение ссылается на другие сигналы по id: `[12]`, `[7]`. Поддерживается
//! простая арифметика, сравнения и логика (приоритет по убыванию):
//!
//! ```text
//! ||                логическое ИЛИ
//! &&                логическое И
//! < <= > >= == !=   сравнения (результат 1.0 / 0.0)
//! + -               сложение/вычитание
//! * /               умножение/деление
//! ! - (унарн.)      отрицание, логическое НЕ
//! ()                группировка
//! AVG([id], ms)     скользящее среднее сигнала за окно миллисекунд
//! ```
//!
//! Числа и результаты — f64; логические/сравнения дают 1.0 (истина) или 0.0
//! (ложь). Если любой операнд `[id]` недоступен (None), результат — None.

use std::collections::HashSet;

/// Верхняя граница окна AVG, мс (10 минут): ограничивает живые буферы
/// вычислителя и размер расширенного окна чтения истории.
pub const MAX_AVG_WINDOW_MS: f64 = 600_000.0;

/// Узел абстрактного синтаксиса.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// Числовая константа.
    Num(f64),
    /// Операнд-ссылка на сигнал по id.
    Ref(i64),
    Neg(Box<Node>),
    Not(Box<Node>),
    Add(Box<Node>, Box<Node>),
    Sub(Box<Node>, Box<Node>),
    Mul(Box<Node>, Box<Node>),
    Div(Box<Node>, Box<Node>),
    Lt(Box<Node>, Box<Node>),
    Le(Box<Node>, Box<Node>),
    Gt(Box<Node>, Box<Node>),
    Ge(Box<Node>, Box<Node>),
    Eq(Box<Node>, Box<Node>),
    Ne(Box<Node>, Box<Node>),
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    /// Скользящее среднее: сигнал `id` за окно `window_ms` миллисекунд,
    /// включая границу `[now - window_ms, now]`.
    Avg { id: i64, window_ms: f64 },
}

/// Одно место в токенах для отчёта об ошибке.
#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
    /// позиция символа в исходной строке (0-based)
    pub pos: usize,
}

impl ParseError {
    fn new(pos: usize, message: impl Into<String>) -> Self {
        Self { message: message.into(), pos }
    }
}

/// Разобрать выражение в AST. Ошибки — с позицией в исходной строке.
pub fn parse(src: &str) -> Result<Node, ParseError> {
    let toks = lex(src)?;
    let mut p = Parser { toks, i: 0 };
    let node = p.parse_or()?;
    if let Some(t) = p.peek() {
        return Err(ParseError::new(t.pos, format!("лишний токен '{:?}'", t.kind)));
    }
    Ok(node)
}

/// Множество id, на которые ссылается выражение (для зависимостей/сортировки).
pub fn dependencies(node: &Node, out: &mut HashSet<i64>) {
    match node {
        Node::Num(_) => {}
        Node::Ref(id) => {
            out.insert(*id);
        }
        Node::Avg { id, .. } => {
            out.insert(*id);
        }
        Node::Neg(a) | Node::Not(a) => dependencies(a, out),
        Node::Add(a, b)
        | Node::Sub(a, b)
        | Node::Mul(a, b)
        | Node::Div(a, b)
        | Node::Lt(a, b)
        | Node::Le(a, b)
        | Node::Gt(a, b)
        | Node::Ge(a, b)
        | Node::Eq(a, b)
        | Node::Ne(a, b)
        | Node::And(a, b)
        | Node::Or(a, b) => {
            dependencies(a, out);
            dependencies(b, out);
        }
    }
}

/// Собрать id операндов из строки выражения (без полного разбора).
/// Возвращает ошибку, если выражение не парсится.
pub fn deps_of(src: &str) -> Result<Vec<i64>, ParseError> {
    let node = parse(src)?;
    let mut set = HashSet::new();
    dependencies(&node, &mut set);
    let mut v: Vec<i64> = set.into_iter().collect();
    v.sort_unstable();
    Ok(v)
}

/// Все AVG-узлы: (id операнда, окно мс). Нужно для живой буферизации окон.
pub fn avg_calls(node: &Node, out: &mut Vec<(i64, f64)>) {
    match node {
        Node::Num(_) | Node::Ref(_) => {}
        Node::Avg { id, window_ms } => out.push((*id, *window_ms)),
        Node::Neg(a) | Node::Not(a) => avg_calls(a, out),
        Node::Add(a, b)
        | Node::Sub(a, b)
        | Node::Mul(a, b)
        | Node::Div(a, b)
        | Node::Lt(a, b)
        | Node::Le(a, b)
        | Node::Gt(a, b)
        | Node::Ge(a, b)
        | Node::Eq(a, b)
        | Node::Ne(a, b)
        | Node::And(a, b)
        | Node::Or(a, b) => {
            avg_calls(a, out);
            avg_calls(b, out);
        }
    }
}

/// Максимальное окно AVG в выражении, мс (0 — функции нет).
pub fn max_window_ms(node: &Node) -> f64 {
    let mut v = Vec::new();
    avg_calls(node, &mut v);
    v.iter().map(|(_, w)| *w).fold(0.0, f64::max)
}

/// Контекст вычисления: откуда брать мгновенные значения операндов и средние
/// по окну для AVG.
pub struct Context<'a> {
    /// Мгновенное значение сигнала (None — недоступен).
    pub lookup: &'a mut dyn FnMut(i64) -> Option<f64>,
    /// Среднее сигнала id по окну window_ms мс перед now_ms (None — нет
    /// источника окна; тогда AVG вырождается в lookup).
    pub avg: Option<&'a mut dyn FnMut(i64, f64) -> Option<f64>>,
    /// Момент «сейчас» для окон AVG, мс эпохи Unix.
    pub now_ms: i64,
}

/// Вычислить AST по контексту (мгновенные значения + окна AVG).
pub fn eval_ctx(node: &Node, cx: &mut Context) -> Option<f64> {
    match node {
        Node::Num(v) => Some(*v),
        Node::Ref(id) => (cx.lookup)(*id),
        Node::Avg { id, window_ms } => match cx.avg.as_mut() {
            Some(w) => (*w)(*id, *window_ms),
            // Нет источника окна (live-реестр без буфера) — среднее по одному
            // текущему отсчёту, т.е. мгновенное значение.
            None => (cx.lookup)(*id),
        },
        Node::Neg(a) => eval_ctx(a, cx).map(|v| -v),
        Node::Not(a) => eval_ctx(a, cx).map(|v| if is_truthy(v) { 0.0 } else { 1.0 }),
        Node::Add(a, b) => eval2_ctx(a, b, cx, |x, y| x + y),
        Node::Sub(a, b) => eval2_ctx(a, b, cx, |x, y| x - y),
        Node::Mul(a, b) => eval2_ctx(a, b, cx, |x, y| x * y),
        Node::Div(a, b) => eval2_ctx(a, b, cx, |x, y| x / y),
        Node::Lt(a, b) => eval2_ctx(a, b, cx, |x, y| bcmp(x < y)),
        Node::Le(a, b) => eval2_ctx(a, b, cx, |x, y| bcmp(x <= y)),
        Node::Gt(a, b) => eval2_ctx(a, b, cx, |x, y| bcmp(x > y)),
        Node::Ge(a, b) => eval2_ctx(a, b, cx, |x, y| bcmp(x >= y)),
        Node::Eq(a, b) => eval2_ctx(a, b, cx, |x, y| bcmp(x == y)),
        Node::Ne(a, b) => eval2_ctx(a, b, cx, |x, y| bcmp(x != y)),
        Node::And(a, b) => {
            let x = eval_ctx(a, cx)?;
            if !is_truthy(x) {
                return Some(0.0);
            }
            let y = eval_ctx(b, cx)?;
            Some(bcmp(is_truthy(y)))
        }
        Node::Or(a, b) => {
            let x = eval_ctx(a, cx)?;
            if is_truthy(x) {
                return Some(1.0);
            }
            let y = eval_ctx(b, cx)?;
            Some(bcmp(is_truthy(y)))
        }
    }
}

fn eval2_ctx(
    a: &Node,
    b: &Node,
    cx: &mut Context,
    f: impl FnOnce(f64, f64) -> f64,
) -> Option<f64> {
    let x = eval_ctx(a, cx)?;
    let y = eval_ctx(b, cx)?;
    Some(f(x, y))
}

/// Вычислить AST, доставая значения операндов через `lookup`.
/// Любой отсутствующий (None) операнд обваливает результат в None.
/// AVG без окна источника вырождается в lookup (один текущий отсчёт).
pub fn eval(node: &Node, lookup: &mut impl FnMut(i64) -> Option<f64>) -> Option<f64> {
    let mut cx = Context {
        lookup: lookup as &mut dyn FnMut(i64) -> Option<f64>,
        avg: None,
        now_ms: 0,
    };
    eval_ctx(node, &mut cx)
}

#[inline]
fn is_truthy(v: f64) -> bool {
    v != 0.0
}

#[inline]
fn bcmp(b: bool) -> f64 {
    if b {
        1.0
    } else {
        0.0
    }
}

// --------------------------------------------------------------- лексер
#[derive(Debug, Clone, PartialEq)]
enum TokKind {
    Num(f64),
    Ref(i64),
    /// Имя функции (ASCII-буквы, регистр не важен).
    Ident(String),
    Comma,
    Plus,
    Minus,
    Star,
    Slash,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
    Not,
    LParen,
    RParen,
}

#[derive(Debug, Clone)]
struct Tok {
    kind: TokKind,
    pos: usize,
}

fn lex(src: &str) -> Result<Vec<Tok>, ParseError> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'0'..=b'9' | b'.' => {
                let start = i;
                // мантисса
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                    i += 1;
                }
                // показатель: e/E [+/-] digits
                if i < b.len() && (b[i] | 0x20) == b'e' {
                    let save = i;
                    i += 1;
                    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
                        i += 1;
                    }
                    if i < b.len() && b[i].is_ascii_digit() {
                        while i < b.len() && b[i].is_ascii_digit() {
                            i += 1;
                        }
                    } else {
                        i = save; // 'e' без цифр — не часть числа
                    }
                }
                let text = &src[start..i];
                let val: f64 = text
                    .parse()
                    .map_err(|_| ParseError::new(start, format!("некорректное число '{text}'")))?;
                out.push(Tok { kind: TokKind::Num(val), pos: start });
            }
            b'[' => {
                let start = i;
                i += 1;
                let ds = i;
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'-') {
                    i += 1;
                }
                if i >= b.len() || b[i] != b']' {
                    return Err(ParseError::new(start, "незакрытая ссылка [id]"));
                }
                let digits = &src[ds..i];
                let id: i64 = digits
                    .parse()
                    .map_err(|_| ParseError::new(start, format!("ссылка '[{digits}]' без числа")))?;
                i += 1; // ']'
                out.push(Tok { kind: TokKind::Ref(id), pos: start });
            }
            b'+' => {
                out.push(Tok { kind: TokKind::Plus, pos: i });
                i += 1;
            }
            b'-' => {
                out.push(Tok { kind: TokKind::Minus, pos: i });
                i += 1;
            }
            b'*' => {
                out.push(Tok { kind: TokKind::Star, pos: i });
                i += 1;
            }
            b'/' => {
                out.push(Tok { kind: TokKind::Slash, pos: i });
                i += 1;
            }
            b'(' => {
                out.push(Tok { kind: TokKind::LParen, pos: i });
                i += 1;
            }
            b')' => {
                out.push(Tok { kind: TokKind::RParen, pos: i });
                i += 1;
            }
            b'!' => {
                if i + 1 < b.len() && b[i + 1] == b'=' {
                    out.push(Tok { kind: TokKind::Ne, pos: i });
                    i += 2;
                } else {
                    out.push(Tok { kind: TokKind::Not, pos: i });
                    i += 1;
                }
            }
            b'=' => {
                if i + 1 < b.len() && b[i + 1] == b'=' {
                    out.push(Tok { kind: TokKind::Eq, pos: i });
                    i += 2;
                } else {
                    return Err(ParseError::new(i, "'=' бывает только '=='"));
                }
            }
            b'<' => {
                if i + 1 < b.len() && b[i + 1] == b'=' {
                    out.push(Tok { kind: TokKind::Le, pos: i });
                    i += 2;
                } else {
                    out.push(Tok { kind: TokKind::Lt, pos: i });
                    i += 1;
                }
            }
            b'>' => {
                if i + 1 < b.len() && b[i + 1] == b'=' {
                    out.push(Tok { kind: TokKind::Ge, pos: i });
                    i += 2;
                } else {
                    out.push(Tok { kind: TokKind::Gt, pos: i });
                    i += 1;
                }
            }
            b'&' => {
                if i + 1 < b.len() && b[i + 1] == b'&' {
                    out.push(Tok { kind: TokKind::And, pos: i });
                    i += 2;
                } else {
                    return Err(ParseError::new(i, "'&' бывает только '&&'"));
                }
            }
            b'|' => {
                if i + 1 < b.len() && b[i + 1] == b'|' {
                    out.push(Tok { kind: TokKind::Or, pos: i });
                    i += 2;
                } else {
                    return Err(ParseError::new(i, "'|' бывает только '||'"));
                }
            }
            b',' => {
                out.push(Tok { kind: TokKind::Comma, pos: i });
                i += 1;
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                out.push(Tok { kind: TokKind::Ident(src[start..i].to_ascii_uppercase()), pos: start });
            }
            other => {
                return Err(ParseError::new(i, format!("неизвестный символ '{}'", other as char)))
            }
        }
    }
    Ok(out)
}

// --------------------------------------------------------------- парсер
struct Parser {
    toks: Vec<Tok>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.i)
    }

    fn eat(&mut self, kind: &TokKind) -> bool {
        if self.peek().map(|t| &t.kind) == Some(kind) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn pos(&self) -> usize {
        self.peek().map(|t| t.pos).unwrap_or(usize::MAX)
    }

    fn expect(&mut self, kind: TokKind) -> Result<(), ParseError> {
        if self.eat(&kind) {
            Ok(())
        } else {
            Err(ParseError::new(self.pos(), format!("ожидалось '{kind:?}'")))
        }
    }

    // or := and (|| and)*
    fn parse_or(&mut self) -> Result<Node, ParseError> {
        let mut left = self.parse_and()?;
        while self.eat(&TokKind::Or) {
            let right = self.parse_and()?;
            left = Node::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    // and := cmp (&& cmp)*
    fn parse_and(&mut self) -> Result<Node, ParseError> {
        let mut left = self.parse_cmp()?;
        while self.eat(&TokKind::And) {
            let right = self.parse_cmp()?;
            left = Node::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    // cmp := add ((< <= > >= == !=) add)?  — один раз (не цепочка), как в C
    fn parse_cmp(&mut self) -> Result<Node, ParseError> {
        let left = self.parse_add()?;
        let node = match self.peek().map(|t| t.kind.clone()) {
            Some(TokKind::Lt) => Some(TokKind::Lt),
            Some(TokKind::Le) => Some(TokKind::Le),
            Some(TokKind::Gt) => Some(TokKind::Gt),
            Some(TokKind::Ge) => Some(TokKind::Ge),
            Some(TokKind::Eq) => Some(TokKind::Eq),
            Some(TokKind::Ne) => Some(TokKind::Ne),
            _ => None,
        };
        match node {
            Some(op) => {
                self.i += 1;
                let right = self.parse_add()?;
                Ok(match op {
                    TokKind::Lt => Node::Lt(Box::new(left), Box::new(right)),
                    TokKind::Le => Node::Le(Box::new(left), Box::new(right)),
                    TokKind::Gt => Node::Gt(Box::new(left), Box::new(right)),
                    TokKind::Ge => Node::Ge(Box::new(left), Box::new(right)),
                    TokKind::Eq => Node::Eq(Box::new(left), Box::new(right)),
                    TokKind::Ne => Node::Ne(Box::new(left), Box::new(right)),
                    _ => unreachable!(),
                })
            }
            None => Ok(left),
        }
    }

    // add := mul ((+ -) mul)*
    fn parse_add(&mut self) -> Result<Node, ParseError> {
        let mut left = self.parse_mul()?;
        loop {
            if self.eat(&TokKind::Plus) {
                let right = self.parse_mul()?;
                left = Node::Add(Box::new(left), Box::new(right));
            } else if self.eat(&TokKind::Minus) {
                let right = self.parse_mul()?;
                left = Node::Sub(Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    // mul := unary ((* /) unary)*
    fn parse_mul(&mut self) -> Result<Node, ParseError> {
        let mut left = self.parse_unary()?;
        loop {
            if self.eat(&TokKind::Star) {
                let right = self.parse_unary()?;
                left = Node::Mul(Box::new(left), Box::new(right));
            } else if self.eat(&TokKind::Slash) {
                let right = self.parse_unary()?;
                left = Node::Div(Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    // unary := ! unary | - unary | primary
    fn parse_unary(&mut self) -> Result<Node, ParseError> {
        if self.eat(&TokKind::Not) {
            let a = self.parse_unary()?;
            return Ok(Node::Not(Box::new(a)));
        }
        if self.eat(&TokKind::Minus) {
            let a = self.parse_unary()?;
            return Ok(Node::Neg(Box::new(a)));
        }
        self.parse_primary()
    }

    // primary := Num | Ref | funcall | ( or )
    // funcall  := AVG ( Ref , Num )
    fn parse_primary(&mut self) -> Result<Node, ParseError> {
        match self.peek().map(|t| t.kind.clone()) {
            Some(TokKind::Ident(name)) => {
                let pos = self.pos();
                self.i += 1;
                if name != "AVG" {
                    return Err(ParseError::new(pos, format!("неизвестная функция '{name}'")));
                }
                self.expect(TokKind::LParen)?;
                let id = match self.peek().map(|t| t.kind.clone()) {
                    Some(TokKind::Ref(id)) => {
                        self.i += 1;
                        id
                    }
                    _ => {
                        return Err(ParseError::new(
                            self.pos(),
                            "у AVG первый аргумент — ссылка вида [id]",
                        ))
                    }
                };
                self.expect(TokKind::Comma)?;
                let window_ms = match self.peek().map(|t| t.kind.clone()) {
                    Some(TokKind::Num(v)) => {
                        self.i += 1;
                        v
                    }
                    _ => {
                        return Err(ParseError::new(
                            self.pos(),
                            "у AVG второй аргумент — окно в миллисекундах",
                        ))
                    }
                };
                self.expect(TokKind::RParen)?;
                if !(window_ms.is_finite() && window_ms > 0.0) {
                    return Err(ParseError::new(pos, "окно AVG должно быть больше 0 мс"));
                }
                if window_ms > MAX_AVG_WINDOW_MS {
                    return Err(ParseError::new(
                        pos,
                        format!("окно AVG {window_ms:.0} мс превышает максимум {MAX_AVG_WINDOW_MS:.0} мс"),
                    ));
                }
                Ok(Node::Avg { id, window_ms })
            }
            Some(TokKind::Num(v)) => {
                self.i += 1;
                Ok(Node::Num(v))
            }
            Some(TokKind::Ref(id)) => {
                self.i += 1;
                Ok(Node::Ref(id))
            }
            Some(TokKind::LParen) => {
                self.i += 1;
                let n = self.parse_or()?;
                self.expect(TokKind::RParen)?;
                Ok(n)
            }
            Some(TokKind::Minus) | Some(TokKind::Plus) => {
                Err(ParseError::new(self.pos(), "бинарный оператор без левого операнда"))
            }
            Some(other) => Err(ParseError::new(self.pos(), format!("неожиданный токен '{other:?}'"))),
            None => Err(ParseError::new(self.pos(), "выражение оборвалось")),
        }
    }
}

// --------------------------------------------------------------- тесты
#[cfg(test)]
mod tests {
    use super::*;

    /// lookup по карте id -> значение.
    fn ev(src: &str, vals: &[(i64, f64)]) -> Option<f64> {
        let node = parse(src).expect("parse");
        let map: std::collections::HashMap<i64, f64> = vals.iter().cloned().collect();
        eval(&node, &mut |id| map.get(&id).copied())
    }

    fn approx(a: Option<f64>, b: f64) -> bool {
        matches!(a, Some(x) if (x - b).abs() < 1e-9)
    }

    #[test]
    fn arithmetic_precedence() {
        assert!(approx(ev("2 + 3 * 4", &[]), 14.0));
        assert!(approx(ev("(2 + 3) * 4", &[]), 20.0));
        assert!(approx(ev("10 / 4", &[]), 2.5));
        assert!(approx(ev("-5 + 2", &[]), -3.0));
        assert!(approx(ev("2 - -3", &[]), 5.0));
    }

    #[test]
    fn refs_eval() {
        assert!(approx(ev("[1] + [2]", &[(1, 10.0), (2, 5.0)]), 15.0));
        assert!(approx(ev("[1] * 0.1 + 20", &[(1, 300.0)]), 50.0));
    }

    #[test]
    fn missing_operand_propagates_none() {
        assert_eq!(ev("[1] + [2]", &[(1, 1.0)]), None);
        assert_eq!(ev("[1] + 5", &[]), None);
    }

    #[test]
    fn comparisons() {
        assert!(approx(ev("[1] > 100", &[(1, 150.0)]), 1.0));
        assert!(approx(ev("[1] > 100", &[(1, 50.0)]), 0.0));
        assert!(approx(ev("[1] >= 100", &[(1, 100.0)]), 1.0));
        assert!(approx(ev("[1] <= 100", &[(1, 100.0)]), 1.0));
        assert!(approx(ev("[1] == 100", &[(1, 100.0)]), 1.0));
        assert!(approx(ev("[1] != 100", &[(1, 100.0)]), 0.0));
    }

    #[test]
    fn logic_short_circuit() {
        // 0 && [неизвестный] => 0 без обращения к операнду
        assert!(approx(ev("[1] && [2]", &[(1, 0.0)]), 0.0));
        // 1 || [неизвестный] => 1
        assert!(approx(ev("[1] || [2]", &[(1, 1.0)]), 1.0));
        assert!(approx(ev("([1] > 80) && ([2] < 50)", &[(1, 90.0), (2, 40.0)]), 1.0));
        assert!(approx(ev("([1] > 80) && ([2] < 50)", &[(1, 90.0), (2, 60.0)]), 0.0));
    }

    #[test]
    fn not_and_neg() {
        assert!(approx(ev("![1]", &[(1, 0.0)]), 1.0));
        assert!(approx(ev("![1]", &[(1, 5.0)]), 0.0));
        assert!(approx(ev("-[1]", &[(1, 3.0)]), -3.0));
    }

    #[test]
    fn dependencies_collected_unique_sorted() {
        assert_eq!(deps_of("[3] + [1] * [3] - [2]").unwrap(), vec![1, 2, 3]);
        assert!(deps_of("1 + 2").unwrap().is_empty());
    }

    #[test]
    fn float_and_scientific_numbers() {
        assert!(approx(ev("1.5 * 2", &[]), 3.0));
        assert!(approx(ev("2e3 + 1", &[]), 2001.0));
        assert!(approx(ev("1.5e-2", &[]), 0.015));
    }

    #[test]
    fn parse_errors() {
        assert!(parse("1 +").is_err());
        assert!(parse("(1 + 2").is_err());
        assert!(parse("[1 + 2]").is_err());
        assert!(parse("1 = 2").is_err());
        assert!(parse("@").is_err());
        assert!(parse("[1] &&").is_err());
    }

    #[test]
    fn error_position_reported() {
        let e = parse("2 + + 3").unwrap_err();
        // второй '+' — бинарный без левого операнда, позиция символа
        assert_eq!(e.pos, 4);
    }

    // ------------------------------------------------------------- AVG
    #[test]
    fn avg_parses_and_fills_deps() {
        assert!(matches!(
            parse("AVG([12], 5000)").unwrap(),
            Node::Avg { id: 12, window_ms: w } if (w - 5000.0).abs() < 1e-9
        ));
        // запись без пробелов должна парситься так же
        assert!(matches!(
            parse("AVG([28],1000)").unwrap(),
            Node::Avg { id: 28, window_ms: w } if (w - 1000.0).abs() < 1e-9
        ));
        // регистронезависимость и в составе выражения
        let n = parse("avg([3], 1e3) + [4]").unwrap();
        assert_eq!(deps_of_node(&n), vec![3, 4]);
        assert!((max_window_ms(&n) - 1000.0).abs() < 1e-9);
    }

    fn deps_of_node(n: &Node) -> Vec<i64> {
        let mut s = HashSet::new();
        dependencies(n, &mut s);
        let mut v: Vec<i64> = s.into_iter().collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn avg_eval_uses_window_source() {
        let node = parse("AVG([12], 100)").unwrap();
        let mut looked = false;
        let mut cx = Context {
            lookup: &mut |_| {
                looked = true;
                Some(99.0)
            },
            avg: Some(&mut |id, w| Some(id as f64 + w)),
            now_ms: 1000,
        };
        assert_eq!(eval_ctx(&node, &mut cx), Some(112.0));
        assert!(!looked); // окно замкнулось на avg-источник
    }

    #[test]
    fn avg_without_window_source_falls_back_to_lookup() {
        assert!(approx(ev("AVG([7], 1000)", &[(7, 3.0)]), 3.0));
    }

    #[test]
    fn avg_parse_errors() {
        assert!(parse("AVG([12])").is_err()); // нет окна
        assert!(parse("AVG(12, 500)").is_err()); // окно, но id без []
        assert!(parse("AVG([12], 0)").is_err()); // нулевое окно
        assert!(parse("AVG([12], 700000)").is_err()); // за максимумом
        assert!(parse("MEAN([12], 5)  ").is_err()); // неизвестная функция
        assert!(parse("AVG [12], 5").is_err()); // без скобок
        assert!(parse("AVG([12], 5,)").is_err());
    }
}
