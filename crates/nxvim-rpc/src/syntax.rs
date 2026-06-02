//! The syntax-worker wire shapes, defined **once** so the server (encoder) and
//! the treesitter worker (decoder) agree by construction. Both crates depend on
//! `nxvim-rpc`, so the tuple arity, field order, and types live here and nowhere
//! else — a change can't desync the two ends into silently-wrong highlights.
//!
//! - [`EditWire`] is the `ts_edit` `edits` 10-tuple (server → worker).
//! - [`SpanWire`] is the `ts_highlights` `spans` 4-tuple (worker → server).

use rmpv::Value;

/// One text delta on the syntax wire, shaped like tree-sitter's `InputEdit`
/// (byte offsets + `(row, col)` points) plus the inserted bytes so the worker's
/// shadow text can be patched. Encoded as a 10-tuple
/// `[start_byte, old_end_byte, new_end_byte, start_row, start_col,
///   old_end_row, old_end_col, new_end_row, new_end_col, text]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditWire {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    /// `(row, byte-column)` at `start_byte`, before the edit.
    pub start_point: (usize, usize),
    /// `(row, byte-column)` at `old_end_byte`, before the edit.
    pub old_end_point: (usize, usize),
    /// `(row, byte-column)` at `new_end_byte`, after the edit.
    pub new_end_point: (usize, usize),
    /// Bytes inserted at `start_byte` (`""` for a deletion).
    pub text: String,
}

impl EditWire {
    pub fn encode(&self) -> Value {
        Value::Array(vec![
            u(self.start_byte),
            u(self.old_end_byte),
            u(self.new_end_byte),
            u(self.start_point.0),
            u(self.start_point.1),
            u(self.old_end_point.0),
            u(self.old_end_point.1),
            u(self.new_end_point.0),
            u(self.new_end_point.1),
            Value::from(self.text.as_str()),
        ])
    }

    /// Decode one wire element, returning `None` if it is not a 10-tuple or any
    /// numeric field is absent/non-integer — coercing a garbled field to `0`
    /// would produce an edit whose bytes and points disagree, desyncing the
    /// worker's shadow from its tree.
    pub fn decode(value: &Value) -> Option<EditWire> {
        let a = value.as_array()?;
        if a.len() != 10 {
            return None;
        }
        let n = |i: usize| Some(a.get(i)?.as_u64()? as usize);
        Some(EditWire {
            start_byte: n(0)?,
            old_end_byte: n(1)?,
            new_end_byte: n(2)?,
            start_point: (n(3)?, n(4)?),
            old_end_point: (n(5)?, n(6)?),
            new_end_point: (n(7)?, n(8)?),
            text: a.get(9).and_then(Value::as_str).unwrap_or("").to_string(),
        })
    }
}

/// One highlight span on the syntax wire: a byte range **within line `line`**
/// and the capture-group name to paint it as. Encoded as a 4-tuple
/// `[line, start_byte, end_byte, group]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanWire {
    pub line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub group: String,
}

impl SpanWire {
    pub fn encode(&self) -> Value {
        Value::Array(vec![
            u(self.line),
            u(self.start_byte),
            u(self.end_byte),
            Value::from(self.group.as_str()),
        ])
    }

    /// Decode one wire element, returning `None` if it is not a 4-tuple or any
    /// numeric field is absent/non-integer.
    pub fn decode(value: &Value) -> Option<SpanWire> {
        let a = value.as_array()?;
        if a.len() != 4 {
            return None;
        }
        let n = |i: usize| Some(a.get(i)?.as_u64()? as usize);
        Some(SpanWire {
            line: n(0)?,
            start_byte: n(1)?,
            end_byte: n(2)?,
            group: a.get(3).and_then(Value::as_str).unwrap_or("").to_string(),
        })
    }
}

/// Encode a slice of edits as the `ts_edit` `edits` array.
pub fn encode_edits(edits: &[EditWire]) -> Value {
    Value::Array(edits.iter().map(EditWire::encode).collect())
}

/// Decode the `ts_edit` `edits` array, dropping any element that doesn't match
/// the wire shape (see [`EditWire::decode`]).
pub fn decode_edits(value: Option<&Value>) -> Vec<EditWire> {
    match value.and_then(Value::as_array) {
        Some(items) => items.iter().filter_map(EditWire::decode).collect(),
        None => Vec::new(),
    }
}

/// Encode a slice of spans as the `ts_highlights` `spans` array.
pub fn encode_spans(spans: &[SpanWire]) -> Value {
    Value::Array(spans.iter().map(SpanWire::encode).collect())
}

fn u(n: usize) -> Value {
    Value::from(n as u64)
}
