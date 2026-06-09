//! A small Vim-expression evaluator, just enough to back `:echo` / `:echomsg`.
//!
//! Vim's `:echo {expr}...` evaluates each space-separated argument as a Vim
//! expression and joins the results with a space — so `:echo "a" "b"` shows
//! `a b`, while `:echo "a" . "b"` (concatenation) shows `ab`. This module covers
//! the subset a config or autocmd realistically puts in an `:echo`: string
//! literals, numbers, concatenation (`.` / `..`), the arithmetic operators, and
//! parentheses.
//!
//! It deliberately does **not** evaluate variables (`g:`, `b:`, `v:…`) or
//! function calls — those live in the Lua runtime, out of reach of pure
//! `nxvim-core`. Rather than silently returning an empty string for them (which
//! would make a typo look like it worked), the evaluator **fails loud**: an
//! identifier or call yields an `E121`-style error the caller surfaces. Routing
//! such references to Lua is a future extension.

use std::fmt::Write as _;

/// A Vim scalar value. Vim distinguishes integers from floats (they print
/// differently and `/` truncates for integers), so we keep them apart.
#[derive(Debug, Clone)]
enum Value {
    Int(i64),
    Float(f64),
    Str(String),
}

impl Value {
    /// The text `:echo` shows for this value. Integers print bare, floats keep a
    /// decimal point (Vim shows `1.0`, not `1`), strings print verbatim.
    fn display(&self) -> String {
        match self {
            Value::Int(n) => n.to_string(),
            Value::Float(f) => fmt_float(*f),
            Value::Str(s) => s.clone(),
        }
    }

    /// Coerce to a number for arithmetic, matching Vim's leading-digits rule:
    /// `"12abc"` is `12`, a non-numeric string is `0`. Floats stay floats.
    fn as_number(&self) -> Value {
        match self {
            Value::Int(_) | Value::Float(_) => self.clone(),
            Value::Str(s) => parse_leading_number(s),
        }
    }
}

/// Format a float the way Vim's `:echo` does: a whole value keeps one trailing
/// zero (`1.0`), otherwise the shortest round-tripping decimal.
fn fmt_float(f: f64) -> String {
    if f.fract() == 0.0 && f.is_finite() {
        let mut s = String::new();
        let _ = write!(s, "{f:.1}");
        s
    } else {
        f.to_string()
    }
}

/// Parse the leading numeric prefix of `s` (Vim's string→number coercion): an
/// optional sign, digits, and an optional fractional part. No numeric prefix is
/// `0`.
fn parse_leading_number(s: &str) -> Value {
    let t = s.trim_start();
    let bytes = t.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'-' || bytes[i] == b'+') {
        i += 1;
    }
    let int_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == int_start {
        return Value::Int(0); // no digits at all
    }
    // Optional fractional part makes it a float.
    let mut is_float = false;
    if i < bytes.len() && bytes[i] == b'.' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
        is_float = true;
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    let slice = &t[..i];
    if is_float {
        Value::Float(slice.parse().unwrap_or(0.0))
    } else {
        Value::Int(slice.parse().unwrap_or(0))
    }
}

/// One lexical token of an `:echo` expression.
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Int(i64),
    Float(f64),
    Str(String),
    /// A bare word — a variable or function name we can't evaluate; kept so the
    /// parser can fail loud naming it.
    Ident(String),
    Concat, // `.` or `..`
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    LParen,
    RParen,
}

/// Tokenize an `:echo` argument string. Errors on an unterminated string.
fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let bytes = src.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b' ' | b'\t' => i += 1,
            b'(' => {
                toks.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                toks.push(Tok::RParen);
                i += 1;
            }
            b'+' => {
                toks.push(Tok::Plus);
                i += 1;
            }
            b'-' => {
                toks.push(Tok::Minus);
                i += 1;
            }
            b'*' => {
                toks.push(Tok::Star);
                i += 1;
            }
            b'/' => {
                toks.push(Tok::Slash);
                i += 1;
            }
            b'%' => {
                toks.push(Tok::Percent);
                i += 1;
            }
            b'.' => {
                // `.` and `..` both concatenate; a `.` directly before a digit is
                // a fractional number only when it follows one, which the number
                // branch handles — a leading `.5` is not valid Vim, so `.` here is
                // always concatenation.
                if i + 1 < bytes.len() && bytes[i + 1] == b'.' {
                    i += 2;
                } else {
                    i += 1;
                }
                toks.push(Tok::Concat);
            }
            b'"' => {
                let (s, next) = lex_double_string(src, i)?;
                toks.push(Tok::Str(s));
                i = next;
            }
            b'\'' => {
                let (s, next) = lex_single_string(src, i)?;
                toks.push(Tok::Str(s));
                i = next;
            }
            b'0'..=b'9' => {
                let (t, next) = lex_number(src, i);
                toks.push(t);
                i = next;
            }
            _ if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b':')
                {
                    i += 1;
                }
                toks.push(Tok::Ident(src[start..i].to_string()));
            }
            _ => {
                return Err(format!(
                    "E15: Invalid expression: unexpected '{}'",
                    c as char
                ))
            }
        }
    }
    Ok(toks)
}

/// Lex a `"…"` string starting at `start` (the opening quote), honoring the
/// common backslash escapes. Returns the decoded contents and the index past the
/// closing quote.
fn lex_double_string(src: &str, start: usize) -> Result<(String, usize), String> {
    let bytes = src.as_bytes();
    let mut i = start + 1;
    let mut out = String::new();
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Ok((out, i + 1)),
            b'\\' if i + 1 < bytes.len() => {
                i += 1;
                match bytes[i] {
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    b'\\' => out.push('\\'),
                    b'"' => out.push('"'),
                    b'0' => out.push('\0'),
                    b'e' => out.push('\x1b'),
                    other => out.push(other as char),
                }
                i += 1;
            }
            other => {
                out.push(other as char);
                i += 1;
            }
        }
    }
    Err("E114: Missing double quote".to_string())
}

/// Lex a `'…'` string starting at `start`. Single-quoted strings are literal;
/// the only escape is `''` for a single quote.
fn lex_single_string(src: &str, start: usize) -> Result<(String, usize), String> {
    let bytes = src.as_bytes();
    let mut i = start + 1;
    let mut out = String::new();
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                out.push('\'');
                i += 2;
            } else {
                return Ok((out, i + 1));
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    Err("E115: Missing single quote".to_string())
}

/// Lex a number (integer or float) starting at `start`. Hex (`0x…`) is read as an
/// integer; a `.` followed by digits makes a float.
fn lex_number(src: &str, start: usize) -> (Tok, usize) {
    let bytes = src.as_bytes();
    // Hex literal.
    if bytes[start] == b'0'
        && start + 1 < bytes.len()
        && (bytes[start + 1] == b'x' || bytes[start + 1] == b'X')
    {
        let mut i = start + 2;
        while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
            i += 1;
        }
        let n = i64::from_str_radix(&src[start + 2..i], 16).unwrap_or(0);
        return (Tok::Int(n), i);
    }
    let mut i = start;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let mut is_float = false;
    if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
        is_float = true;
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    if is_float {
        (Tok::Float(src[start..i].parse().unwrap_or(0.0)), i)
    } else {
        (Tok::Int(src[start..i].parse().unwrap_or(0)), i)
    }
}

/// Recursive-descent parser/evaluator over the token stream.
struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    /// `:echo` joins each top-level expression with a space.
    fn eval_all(&mut self) -> Result<String, String> {
        let mut parts = Vec::new();
        while self.peek().is_some() {
            parts.push(self.expr()?.display());
        }
        Ok(parts.join(" "))
    }

    /// Lowest precedence: string concatenation (`.` / `..`).
    fn expr(&mut self) -> Result<Value, String> {
        let mut left = self.additive()?;
        while matches!(self.peek(), Some(Tok::Concat)) {
            self.next();
            let right = self.additive()?;
            left = Value::Str(format!("{}{}", left.display(), right.display()));
        }
        Ok(left)
    }

    fn additive(&mut self) -> Result<Value, String> {
        let mut left = self.multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Plus) => 1,
                Some(Tok::Minus) => -1,
                _ => break,
            };
            self.next();
            let right = self.multiplicative()?;
            left = arith(&left, &right, if op == 1 { '+' } else { '-' })?;
        }
        Ok(left)
    }

    fn multiplicative(&mut self) -> Result<Value, String> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Star) => '*',
                Some(Tok::Slash) => '/',
                Some(Tok::Percent) => '%',
                _ => break,
            };
            self.next();
            let right = self.unary()?;
            left = arith(&left, &right, op)?;
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Value, String> {
        if matches!(self.peek(), Some(Tok::Minus)) {
            self.next();
            let v = self.unary()?;
            return arith(&Value::Int(0), &v, '-');
        }
        if matches!(self.peek(), Some(Tok::Plus)) {
            self.next();
            return self.unary();
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Value, String> {
        match self.next() {
            Some(Tok::Int(n)) => Ok(Value::Int(n)),
            Some(Tok::Float(f)) => Ok(Value::Float(f)),
            Some(Tok::Str(s)) => Ok(Value::Str(s)),
            Some(Tok::LParen) => {
                let v = self.expr()?;
                match self.next() {
                    Some(Tok::RParen) => Ok(v),
                    _ => Err("E110: Missing ')'".to_string()),
                }
            }
            // A bare word is a variable or function — not evaluable in core. Fail
            // loud naming it rather than pretending it was the empty string.
            Some(Tok::Ident(name)) => Err(format!(
                "E121: Undefined variable: {name} (:echo supports only literals \
                 and arithmetic, not variables/functions)"
            )),
            Some(t) => Err(format!("E15: Invalid expression: unexpected {t:?}")),
            None => Err("E15: Invalid expression: unexpected end".to_string()),
        }
    }
}

/// Apply a numeric operator, following Vim's int/float rules: an operation on two
/// integers stays an integer (with `/` truncating toward zero), and any float
/// operand promotes the result to float.
fn arith(a: &Value, b: &Value, op: char) -> Result<Value, String> {
    let a = a.as_number();
    let b = b.as_number();
    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
        let (x, y) = (*x, *y);
        return Ok(Value::Int(match op {
            '+' => x.wrapping_add(y),
            '-' => x.wrapping_sub(y),
            '*' => x.wrapping_mul(y),
            '/' => {
                if y == 0 {
                    return Err("E1154: Divide by zero".to_string());
                }
                x / y // Rust integer division truncates toward zero, like Vim
            }
            '%' => {
                if y == 0 {
                    return Err("E1154: Divide by zero".to_string());
                }
                x % y
            }
            _ => unreachable!(),
        }));
    }
    let x = to_f64(&a);
    let y = to_f64(&b);
    Ok(Value::Float(match op {
        '+' => x + y,
        '-' => x - y,
        '*' => x * y,
        '/' => x / y,
        '%' => x % y,
        _ => unreachable!(),
    }))
}

fn to_f64(v: &Value) -> f64 {
    match v {
        Value::Int(n) => *n as f64,
        Value::Float(f) => *f,
        Value::Str(_) => match v.as_number() {
            Value::Int(n) => n as f64,
            Value::Float(f) => f,
            Value::Str(_) => 0.0,
        },
    }
}

/// Evaluate an `:echo` argument list, returning the text to display (the
/// space-joined results) or a Vim-style error message.
pub(crate) fn eval_echo(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let toks = tokenize(trimmed)?;
    let mut p = Parser { toks, pos: 0 };
    p.eval_all()
}
