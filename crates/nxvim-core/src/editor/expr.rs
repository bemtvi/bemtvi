//! A small Vim-expression evaluator, shared by `:echo` / `:echomsg` and the
//! `%{…}` items of a `'statusline'`.
//!
//! Vim's `:echo {expr}...` evaluates each space-separated argument as a Vim
//! expression and joins the results with a space — so `:echo "a" "b"` shows
//! `a b`, while `:echo "a" . "b"` (concatenation) shows `ab`. This module covers
//! the subset a config or autocmd realistically puts in an `:echo` or a
//! statusline: string literals, numbers, concatenation (`.` / `..`), the
//! arithmetic operators, the comparison operators (`==` `!=` `<` `<=` `>` `>=`),
//! the logical operators (`&&` `||` `!`), the ternary `a ? b : c`, parentheses,
//! and — when the caller supplies an option resolver — `&option` references.
//!
//! It deliberately does **not** evaluate variables (`g:`, `b:`, `v:…`) or
//! function calls — those live in the Lua runtime, out of reach of pure
//! `nxvim-core`. Rather than silently returning an empty string for them (which
//! would make a typo look like it worked), the evaluator **fails loud**: an
//! identifier or call yields an `E121`-style error the caller surfaces. An
//! `&option` with no resolver (or an unknown option name) fails loud the same way
//! rather than expanding to nothing.

use std::fmt::Write as _;

/// A resolved `&option` value, as handed back by the resolver `eval_expr` is
/// given. Vim options are either a number (`&bomb` → `0`/`1`) or a string
/// (`&fileencoding` → `"utf-8"`); booleans arrive as `Int(0|1)`.
pub enum OptVal {
    Int(i64),
    Str(String),
}

/// A caller-supplied resolver from an `&option` name to its value, threaded into
/// the evaluator so `&fileencoding` / `&bomb` / … render in a statusline `%{…}`.
pub type OptResolver<'a> = &'a dyn Fn(&str) -> Option<OptVal>;

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
    /// An `&option` reference (the leading `&` and any `l:`/`g:` scope stripped),
    /// resolved by the caller-supplied resolver.
    Option(String),
    Concat, // `.` or `..`
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    LParen,
    RParen,
    Eq, // `==`
    Ne, // `!=`
    Lt, // `<`
    Le, // `<=`
    Gt, // `>`
    Ge, // `>=`
    Bang,
    AndAnd,
    OrOr,
    Question,
    Colon,
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
            b'=' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    toks.push(Tok::Eq);
                    i += 2;
                } else {
                    return Err("E15: Invalid expression: '=' (use '==')".to_string());
                }
            }
            b'!' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    toks.push(Tok::Ne);
                    i += 2;
                } else {
                    toks.push(Tok::Bang);
                    i += 1;
                }
            }
            b'<' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    toks.push(Tok::Le);
                    i += 2;
                } else {
                    toks.push(Tok::Lt);
                    i += 1;
                }
            }
            b'>' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    toks.push(Tok::Ge);
                    i += 2;
                } else {
                    toks.push(Tok::Gt);
                    i += 1;
                }
            }
            b'?' => {
                toks.push(Tok::Question);
                i += 1;
            }
            b':' => {
                toks.push(Tok::Colon);
                i += 1;
            }
            b'|' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'|' {
                    toks.push(Tok::OrOr);
                    i += 2;
                } else {
                    return Err("E15: Invalid expression: '|' (use '||')".to_string());
                }
            }
            b'&' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'&' {
                    toks.push(Tok::AndAnd);
                    i += 2;
                } else {
                    let (name, next) = lex_option(src, i)?;
                    toks.push(Tok::Option(name));
                    i = next;
                }
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
                // Report the whole (possibly multi-byte) char, not the lead byte
                // reinterpreted as Latin-1.
                let ch = src[i..].chars().next().unwrap();
                return Err(format!("E15: Invalid expression: unexpected '{ch}'"));
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
                    // An unrecognized escape keeps the escaped char itself —
                    // the whole char, which may be multi-byte (`"\é"` is `é`),
                    // never the lead byte reinterpreted as Latin-1.
                    _ => {
                        let ch = src[i..].chars().next().unwrap();
                        out.push(ch);
                        i += ch.len_utf8();
                        continue;
                    }
                }
                i += 1;
            }
            _ => {
                // Copy the whole (possibly multi-byte) char; pushing the byte
                // `as char` would Latin-1-mangle UTF-8 (`héllo` → `hÃ©llo`).
                let ch = src[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
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
            // Whole (possibly multi-byte) char, not `byte as char` (Latin-1).
            let ch = src[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
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

/// Lex an `&option` reference starting at `start` (the `&`). An optional `l:` /
/// `g:` scope is accepted and discarded — a statusline wants the effective value
/// either way. The name is the run of letters that follows. Returns the bare
/// option name and the index past it.
fn lex_option(src: &str, start: usize) -> Result<(String, usize), String> {
    let bytes = src.as_bytes();
    let mut i = start + 1; // past the '&'

    // Discard a `l:` / `g:` scope prefix if present.
    if i + 1 < bytes.len() && (bytes[i] == b'l' || bytes[i] == b'g') && bytes[i + 1] == b':' {
        i += 2;
    }
    let name_start = i;
    while i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
        i += 1;
    }
    if i == name_start {
        return Err("E15: Invalid expression: '&' with no option name".to_string());
    }
    Ok((src[name_start..i].to_string(), i))
}

/// Recursive-descent parser/evaluator over the token stream.
struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    /// Resolver for `&option` references. `None` (e.g. `:echo`) means options are
    /// not available here and any `&option` fails loud.
    opts: Option<OptResolver<'a>>,
}

impl<'a> Parser<'a> {
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
            parts.push(self.ternary()?.display());
        }
        Ok(parts.join(" "))
    }

    /// Lowest precedence: the ternary `cond ? then : else`. Right-associative, so
    /// `a ? b : c ? d : e` parses as `a ? b : (c ? d : e)`.
    fn ternary(&mut self) -> Result<Value, String> {
        let cond = self.logic_or()?;
        if matches!(self.peek(), Some(Tok::Question)) {
            self.next();
            let then_branch = self.ternary()?;
            match self.next() {
                Some(Tok::Colon) => {}
                _ => return Err("E109: Missing ':' after '?'".to_string()),
            }
            let else_branch = self.ternary()?;
            Ok(if truthy(&cond) {
                then_branch
            } else {
                else_branch
            })
        } else {
            Ok(cond)
        }
    }

    /// `||` — Vim yields `0`/`1`, short-circuiting the right side.
    fn logic_or(&mut self) -> Result<Value, String> {
        let mut left = self.logic_and()?;
        while matches!(self.peek(), Some(Tok::OrOr)) {
            self.next();
            let right = self.logic_and()?;
            left = Value::Int((truthy(&left) || truthy(&right)) as i64);
        }
        Ok(left)
    }

    /// `&&` — Vim yields `0`/`1`.
    fn logic_and(&mut self) -> Result<Value, String> {
        let mut left = self.comparison()?;
        while matches!(self.peek(), Some(Tok::AndAnd)) {
            self.next();
            let right = self.comparison()?;
            left = Value::Int((truthy(&left) && truthy(&right)) as i64);
        }
        Ok(left)
    }

    /// Comparison operators, non-associative in Vim but we parse left-to-right.
    /// The result is `0`/`1`. Two strings compare lexically (case-sensitive, like
    /// Vim's `==#`); otherwise both sides coerce to numbers.
    fn comparison(&mut self) -> Result<Value, String> {
        let left = self.concat()?;
        let op = match self.peek() {
            Some(Tok::Eq) => '=',
            Some(Tok::Ne) => '!',
            Some(Tok::Lt) => '<',
            Some(Tok::Le) => 'l',
            Some(Tok::Gt) => '>',
            Some(Tok::Ge) => 'g',
            _ => return Ok(left),
        };
        self.next();
        let right = self.concat()?;
        Ok(Value::Int(compare(&left, &right, op) as i64))
    }

    /// String concatenation (`.` / `..`).
    fn concat(&mut self) -> Result<Value, String> {
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
        if matches!(self.peek(), Some(Tok::Bang)) {
            self.next();
            let v = self.unary()?;
            return Ok(Value::Int(!truthy(&v) as i64));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Value, String> {
        match self.next() {
            Some(Tok::Int(n)) => Ok(Value::Int(n)),
            Some(Tok::Float(f)) => Ok(Value::Float(f)),
            Some(Tok::Str(s)) => Ok(Value::Str(s)),
            Some(Tok::LParen) => {
                let v = self.ternary()?;
                match self.next() {
                    Some(Tok::RParen) => Ok(v),
                    _ => Err("E110: Missing ')'".to_string()),
                }
            }
            // `&option` — resolved by the caller. Without a resolver, or for an
            // unknown name, fail loud rather than expanding to nothing.
            Some(Tok::Option(name)) => match self.opts {
                Some(resolve) => match resolve(&name) {
                    Some(OptVal::Int(n)) => Ok(Value::Int(n)),
                    Some(OptVal::Str(s)) => Ok(Value::Str(s)),
                    None => Err(format!("E518: Unknown option: {name}")),
                },
                None => Err(format!(
                    "E15: Invalid expression: &{name} (option references are not \
                     available here)"
                )),
            },
            // A bare word is a variable or function — not evaluable in core (no
            // Vimscript variables). Fail loud naming it, with the standard Vim
            // message kept short so it stays legible when a statusline truncates.
            Some(Tok::Ident(name)) => Err(format!("E121: Undefined variable: {name}")),
            Some(t) => Err(format!("E15: Invalid expression: unexpected {t:?}")),
            None => Err("E15: Invalid expression: unexpected end".to_string()),
        }
    }
}

/// Vim truthiness: a value is true when it coerces to a non-zero number. A
/// non-numeric string (`"utf-8"`) is `0` → false.
fn truthy(v: &Value) -> bool {
    match v.as_number() {
        Value::Int(n) => n != 0,
        Value::Float(f) => f != 0.0,
        Value::Str(_) => false,
    }
}

/// Apply a comparison operator (`'='`==, `'!'`!=, `'<'`, `'l'`<=, `'>'`, `'g'`>=),
/// returning the boolean result. Two strings compare lexically (case-sensitive);
/// any other mix coerces both operands to numbers, matching Vim.
fn compare(a: &Value, b: &Value, op: char) -> bool {
    let ord = if let (Value::Str(x), Value::Str(y)) = (a, b) {
        x.cmp(y)
    } else {
        to_f64(a)
            .partial_cmp(&to_f64(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    };
    use std::cmp::Ordering::*;
    match op {
        '=' => ord == Equal,
        '!' => ord != Equal,
        '<' => ord == Less,
        'l' => ord != Greater,
        '>' => ord == Greater,
        'g' => ord != Less,
        _ => unreachable!(),
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
/// space-joined results) or a Vim-style error message. `&option` references are
/// not available here (no resolver) and fail loud.
pub(crate) fn eval_echo(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let toks = tokenize(trimmed)?;
    let mut p = Parser {
        toks,
        pos: 0,
        opts: None,
    };
    p.eval_all()
}

/// Evaluate a single Vim expression (the body of a statusline `%{…}`), resolving
/// `&option` references through `resolve_opt`. Returns the rendered text or a
/// Vim-style error message. Unlike [`eval_echo`], the whole input is one
/// expression — there is no space-join of multiple top-level terms.
pub fn eval_expr(input: &str, resolve_opt: OptResolver) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let toks = tokenize(trimmed)?;
    let mut p = Parser {
        toks,
        pos: 0,
        opts: Some(resolve_opt),
    };
    let value = p.ternary()?;
    if p.peek().is_some() {
        return Err(format!(
            "E15: Invalid expression: trailing tokens after a complete expression in {trimmed:?}"
        ));
    }
    Ok(value.display())
}
