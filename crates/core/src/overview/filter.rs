//! Attribute (row) filtering — `--filter` / `--where` (#315).
//!
//! A small SQL-WHERE-style predicate over the input's property columns,
//! evaluated during the pass-1 scan so it composes with `--bbox` and feeds
//! the same downstream pipeline: a row the predicate does not accept simply
//! never produces an [`AssignFeature`], exactly like a bbox miss.
//!
//! # Grammar (hand-rolled recursive descent — no new dependencies)
//!
//! ```text
//! expr      := or_expr
//! or_expr   := and_expr ( OR and_expr )*
//! and_expr  := unary ( AND unary )*
//! unary     := NOT unary | '(' expr ')' | predicate
//! predicate := column cmp_op literal
//!            | column [NOT] IN '(' literal ( ',' literal )* ')'
//!            | column IS [NOT] NULL
//! cmp_op    := '=' | '==' | '!=' | '<>' | '<' | '<=' | '>' | '>='
//! column    := bare identifier | "double-quoted identifier"
//! literal   := number | 'single-quoted string' | TRUE | FALSE
//! ```
//!
//! Keywords are case-insensitive. The column must be on the left-hand side
//! of a comparison. String literals escape an embedded quote by doubling it
//! (`'it''s'`). `= NULL` is rejected with a hint to use `IS NULL`.
//!
//! # Null semantics (SQL three-valued logic)
//!
//! Comparisons and `IN` over a NULL value yield UNKNOWN, `AND`/`OR`/`NOT`
//! combine with Kleene logic, and a row is kept only when the whole
//! expression evaluates to TRUE — so `confidence > 0.8` drops null-confidence
//! rows, and `NOT (confidence > 0.8)` drops them too (UNKNOWN, not TRUE).
//! `IS NULL` / `IS NOT NULL` are the explicit null tests and always yield
//! TRUE/FALSE.
//!
//! # Row-group statistics pushdown
//!
//! [`BoundFilter::select_row_groups`] prunes input row groups whose parquet
//! column chunk statistics (min/max/null-count) prove the predicate cannot
//! match any row — footer-only, no data pages read, which on remote input
//! means the pruned byte ranges are never fetched. The test is conservative:
//! missing or non-prunable statistics (and every `NOT (...)` subtree) keep
//! the row group, and the exact per-row evaluation in pass 1 guarantees
//! identical output either way. Statistics min/max are valid bounds even
//! when writers truncate them, so bound-based pruning stays correct.
//!
//! Numeric comparisons are performed in `f64` (matching the ranking path's
//! [`extract_sort_keys`]); Int64 statistics outside the exact-`f64` range
//! (|v| >= 2^53) are widened before pruning so rounding can never prune a
//! matching row group.

use std::collections::HashMap;

use arrow_array::cast::AsArray;
use arrow_array::{Array, RecordBatch};
use arrow_schema::{DataType, Schema, TimeUnit};
use parquet::file::metadata::{ParquetMetaData, RowGroupMetaData};
use parquet::file::statistics::Statistics;

use super::convert::extract_sort_keys;

/// Errors from parsing or binding a `--filter` expression.
#[derive(Debug, thiserror::Error)]
pub enum FilterError {
    /// The expression source failed to parse.
    #[error("invalid --filter expression: {0}")]
    Parse(String),
    /// The expression references a column absent from the input schema.
    #[error("--filter references unknown column {name:?} (input columns: {available})")]
    UnknownColumn {
        /// The unresolved column name.
        name: String,
        /// Comma-joined available column names, for the error message.
        available: String,
    },
    /// The referenced column has a type the filter cannot evaluate.
    #[error(
        "--filter column {name:?} has unsupported type {data_type} \
         (supported: numeric, string, boolean, timestamp)"
    )]
    UnsupportedColumnType {
        /// The column name.
        name: String,
        /// The Arrow data type found.
        data_type: String,
    },
    /// A literal's type does not match the column's type.
    #[error("--filter: cannot compare {kind} column {name:?} to {literal}")]
    TypeMismatch {
        /// The column name.
        name: String,
        /// The column kind ("numeric" / "string" / "boolean").
        kind: &'static str,
        /// Description of the offending literal.
        literal: String,
    },
    /// An ordering comparison (`<`, `<=`, `>`, `>=`) on a boolean column.
    #[error("--filter: ordering comparison on boolean column {name:?} is not supported")]
    BooleanOrdering {
        /// The column name.
        name: String,
    },
    /// A string literal compared against a timestamp column failed to parse
    /// as a datetime.
    #[error(
        "--filter: cannot parse {literal:?} as a datetime for timestamp column {name:?} \
         (accepted: '2025-01-01', '2025-01-01 12:30:00', RFC 3339): {msg}"
    )]
    TimestampLiteral {
        /// The column name.
        name: String,
        /// The offending literal source text.
        literal: String,
        /// The underlying parse error.
        msg: String,
    },
}

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `=` / `==`
    Eq,
    /// `!=` / `<>`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

/// A literal value in a filter expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// A numeric literal (all numerics are `f64`).
    Number(f64),
    /// A single-quoted string literal.
    String(String),
    /// `TRUE` / `FALSE`.
    Bool(bool),
    /// A datetime literal as epoch nanoseconds (UTC). Never produced by the
    /// parser: binding coerces a string literal compared against a timestamp
    /// column into this form (`'2025-01-01'`, `'2025-01-01 12:30:00'`,
    /// RFC 3339 with offset, ...). Timezone-less literals are read as UTC.
    Timestamp(i64),
}

impl Literal {
    fn describe(&self) -> String {
        match self {
            Literal::Number(v) => format!("number {v}"),
            Literal::String(s) => format!("string {s:?}"),
            Literal::Bool(b) => format!("boolean {b}"),
            Literal::Timestamp(ns) => format!("timestamp {ns}ns"),
        }
    }
}

/// A parsed (unbound) filter expression AST.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterExpr {
    /// `column op literal`
    Compare {
        /// Left-hand column name.
        column: String,
        /// Comparison operator.
        op: CmpOp,
        /// Right-hand literal.
        value: Literal,
    },
    /// `column [NOT] IN (l1, l2, ...)`
    In {
        /// Column name.
        column: String,
        /// The literal list (non-empty).
        values: Vec<Literal>,
        /// `NOT IN` when true.
        negated: bool,
    },
    /// `column IS [NOT] NULL`
    IsNull {
        /// Column name.
        column: String,
        /// `IS NOT NULL` when true.
        negated: bool,
    },
    /// `NOT expr`
    Not(Box<FilterExpr>),
    /// `a AND b`
    And(Box<FilterExpr>, Box<FilterExpr>),
    /// `a OR b`
    Or(Box<FilterExpr>, Box<FilterExpr>),
}

/// Parse a filter expression source string into a [`FilterExpr`].
pub fn parse_filter(src: &str) -> Result<FilterExpr, FilterError> {
    let tokens = tokenize(src)?;
    let mut p = Parser { tokens, pos: 0 };
    let expr = p.parse_or()?;
    if p.pos != p.tokens.len() {
        return Err(FilterError::Parse(format!(
            "unexpected trailing input at {:?}",
            p.peek_desc()
        )));
    }
    Ok(expr)
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Number(f64),
    Str(String),
    LParen,
    RParen,
    Comma,
    Op(CmpOp),
    And,
    Or,
    Not,
    In,
    Is,
    Null,
    True,
    False,
}

fn tokenize(src: &str) -> Result<Vec<Token>, FilterError> {
    let mut out = Vec::new();
    let b = src.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i] as char;
        match c {
            ' ' | '\t' | '\r' | '\n' => i += 1,
            '(' => {
                out.push(Token::LParen);
                i += 1;
            }
            ')' => {
                out.push(Token::RParen);
                i += 1;
            }
            ',' => {
                out.push(Token::Comma);
                i += 1;
            }
            '=' => {
                i += if b.get(i + 1) == Some(&b'=') { 2 } else { 1 };
                out.push(Token::Op(CmpOp::Eq));
            }
            '!' => {
                if b.get(i + 1) == Some(&b'=') {
                    out.push(Token::Op(CmpOp::Ne));
                    i += 2;
                } else {
                    return Err(FilterError::Parse(
                        "'!' must be followed by '=' (use NOT for negation)".into(),
                    ));
                }
            }
            '<' => match b.get(i + 1) {
                Some(&b'=') => {
                    out.push(Token::Op(CmpOp::Le));
                    i += 2;
                }
                Some(&b'>') => {
                    out.push(Token::Op(CmpOp::Ne));
                    i += 2;
                }
                _ => {
                    out.push(Token::Op(CmpOp::Lt));
                    i += 1;
                }
            },
            '>' => {
                if b.get(i + 1) == Some(&b'=') {
                    out.push(Token::Op(CmpOp::Ge));
                    i += 2;
                } else {
                    out.push(Token::Op(CmpOp::Gt));
                    i += 1;
                }
            }
            '\'' => {
                // Single-quoted string; '' escapes an embedded quote.
                let mut s = String::new();
                i += 1;
                loop {
                    match b.get(i) {
                        None => {
                            return Err(FilterError::Parse("unterminated string literal".into()))
                        }
                        Some(&b'\'') => {
                            if b.get(i + 1) == Some(&b'\'') {
                                s.push('\'');
                                i += 2;
                            } else {
                                i += 1;
                                break;
                            }
                        }
                        Some(_) => {
                            // Advance by whole UTF-8 chars.
                            let ch = src[i..].chars().next().expect("in-bounds char");
                            s.push(ch);
                            i += ch.len_utf8();
                        }
                    }
                }
                out.push(Token::Str(s));
            }
            '"' => {
                // Double-quoted identifier; "" escapes an embedded quote.
                let mut s = String::new();
                i += 1;
                loop {
                    match b.get(i) {
                        None => {
                            return Err(FilterError::Parse("unterminated quoted identifier".into()))
                        }
                        Some(&b'"') => {
                            if b.get(i + 1) == Some(&b'"') {
                                s.push('"');
                                i += 2;
                            } else {
                                i += 1;
                                break;
                            }
                        }
                        Some(_) => {
                            let ch = src[i..].chars().next().expect("in-bounds char");
                            s.push(ch);
                            i += ch.len_utf8();
                        }
                    }
                }
                out.push(Token::Ident(s));
            }
            '-' | '+' | '0'..='9' | '.' => {
                let start = i;
                i += 1; // sign or first digit
                while i < b.len() && matches!(b[i] as char, '0'..='9' | '.' | 'e' | 'E') {
                    // Exponent sign: 1e-3
                    if matches!(b[i] as char, 'e' | 'E')
                        && matches!(b.get(i + 1), Some(&b'-' | &b'+'))
                    {
                        i += 1;
                    }
                    i += 1;
                }
                let text = &src[start..i];
                let v: f64 = text
                    .parse()
                    .map_err(|_| FilterError::Parse(format!("invalid number literal {text:?}")))?;
                out.push(Token::Number(v));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < b.len() && ((b[i] as char).is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                let word = &src[start..i];
                out.push(match word.to_ascii_uppercase().as_str() {
                    "AND" => Token::And,
                    "OR" => Token::Or,
                    "NOT" => Token::Not,
                    "IN" => Token::In,
                    "IS" => Token::Is,
                    "NULL" => Token::Null,
                    "TRUE" => Token::True,
                    "FALSE" => Token::False,
                    _ => Token::Ident(word.to_string()),
                });
            }
            other => {
                return Err(FilterError::Parse(format!(
                    "unexpected character {other:?}"
                )))
            }
        }
    }
    if out.is_empty() {
        return Err(FilterError::Parse("empty expression".into()));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn peek_desc(&self) -> String {
        match self.peek() {
            Some(t) => format!("{t:?}"),
            None => "end of input".to_string(),
        }
    }

    fn eat(&mut self, t: &Token) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: Token, what: &str) -> Result<(), FilterError> {
        if self.eat(&t) {
            Ok(())
        } else {
            Err(FilterError::Parse(format!(
                "expected {what}, found {}",
                self.peek_desc()
            )))
        }
    }

    fn parse_or(&mut self) -> Result<FilterExpr, FilterError> {
        let mut left = self.parse_and()?;
        while self.eat(&Token::Or) {
            let right = self.parse_and()?;
            left = FilterExpr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<FilterExpr, FilterError> {
        let mut left = self.parse_unary()?;
        while self.eat(&Token::And) {
            let right = self.parse_unary()?;
            left = FilterExpr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<FilterExpr, FilterError> {
        if self.eat(&Token::Not) {
            return Ok(FilterExpr::Not(Box::new(self.parse_unary()?)));
        }
        if self.eat(&Token::LParen) {
            let e = self.parse_or()?;
            self.expect(Token::RParen, "')'")?;
            return Ok(e);
        }
        self.parse_predicate()
    }

    fn parse_predicate(&mut self) -> Result<FilterExpr, FilterError> {
        let column = match self.peek().cloned() {
            Some(Token::Ident(name)) => {
                self.pos += 1;
                name
            }
            _ => {
                return Err(FilterError::Parse(format!(
                    "expected a column name, found {}",
                    self.peek_desc()
                )))
            }
        };
        match self.peek().cloned() {
            Some(Token::Op(op)) => {
                self.pos += 1;
                let value = self.parse_literal()?;
                Ok(FilterExpr::Compare { column, op, value })
            }
            Some(Token::In) => {
                self.pos += 1;
                self.parse_in_list(column, false)
            }
            Some(Token::Not) => {
                self.pos += 1;
                self.expect(Token::In, "IN after NOT")?;
                self.parse_in_list(column, true)
            }
            Some(Token::Is) => {
                self.pos += 1;
                let negated = self.eat(&Token::Not);
                self.expect(Token::Null, "NULL after IS")?;
                Ok(FilterExpr::IsNull { column, negated })
            }
            _ => Err(FilterError::Parse(format!(
                "expected a comparison operator, IN, or IS after column \
                 {column:?}, found {}",
                self.peek_desc()
            ))),
        }
    }

    fn parse_in_list(&mut self, column: String, negated: bool) -> Result<FilterExpr, FilterError> {
        self.expect(Token::LParen, "'(' after IN")?;
        let mut values = vec![self.parse_literal()?];
        while self.eat(&Token::Comma) {
            values.push(self.parse_literal()?);
        }
        self.expect(Token::RParen, "')' closing the IN list")?;
        Ok(FilterExpr::In {
            column,
            values,
            negated,
        })
    }

    fn parse_literal(&mut self) -> Result<Literal, FilterError> {
        match self.peek().cloned() {
            Some(Token::Number(v)) => {
                self.pos += 1;
                Ok(Literal::Number(v))
            }
            Some(Token::Str(s)) => {
                self.pos += 1;
                Ok(Literal::String(s))
            }
            Some(Token::True) => {
                self.pos += 1;
                Ok(Literal::Bool(true))
            }
            Some(Token::False) => {
                self.pos += 1;
                Ok(Literal::Bool(false))
            }
            Some(Token::Null) => Err(FilterError::Parse(
                "NULL is not a comparable value; use IS NULL / IS NOT NULL".into(),
            )),
            _ => Err(FilterError::Parse(format!(
                "expected a literal (number, 'string', TRUE, FALSE), found {}",
                self.peek_desc()
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Binding
// ---------------------------------------------------------------------------

/// The evaluation type class of a filter column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColKind {
    Num,
    Str,
    Bool,
    /// Timestamp column of the given Arrow unit. Timezone metadata is
    /// irrelevant to filtering: Arrow timestamps store an epoch instant, and
    /// datetime literals are read as UTC.
    Ts(TimeUnit),
}

impl ColKind {
    fn name(self) -> &'static str {
        match self {
            ColKind::Num => "numeric",
            ColKind::Str => "string",
            ColKind::Bool => "boolean",
            ColKind::Ts(_) => "timestamp",
        }
    }
}

fn column_kind(dt: &DataType) -> Option<ColKind> {
    match dt {
        DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32
        | DataType::UInt64
        | DataType::Float32
        | DataType::Float64 => Some(ColKind::Num),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => Some(ColKind::Str),
        DataType::Boolean => Some(ColKind::Bool),
        DataType::Timestamp(unit, _) => Some(ColKind::Ts(*unit)),
        DataType::Dictionary(_, inner) => match inner.as_ref() {
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => Some(ColKind::Str),
            _ => None,
        },
        _ => None,
    }
}

/// Nanoseconds per unit tick, for widening a unit value into epoch nanos.
/// Comparisons happen in `i128` so the widening can never overflow.
fn ts_factor(unit: TimeUnit) -> i128 {
    match unit {
        TimeUnit::Second => 1_000_000_000,
        TimeUnit::Millisecond => 1_000_000,
        TimeUnit::Microsecond => 1_000,
        TimeUnit::Nanosecond => 1,
    }
}

/// One bound column reference: index into the (possibly reserved-renamed)
/// input schema for evaluation, plus the ORIGINAL parquet column name for
/// statistics pushdown (the file's footer always carries pre-rename names).
#[derive(Debug, Clone)]
struct BoundCol {
    idx: usize,
    parquet_name: String,
    kind: ColKind,
}

#[derive(Debug, Clone)]
enum BoundExpr {
    Compare {
        col: BoundCol,
        op: CmpOp,
        value: Literal,
    },
    In {
        col: BoundCol,
        values: Vec<Literal>,
        negated: bool,
    },
    IsNull {
        col: BoundCol,
        negated: bool,
    },
    Not(Box<BoundExpr>),
    And(Box<BoundExpr>, Box<BoundExpr>),
    Or(Box<BoundExpr>, Box<BoundExpr>),
}

/// A filter expression bound to an input schema: column names resolved to
/// indices, literal types checked against column types. Ready for per-batch
/// evaluation ([`Self::eval_mask`]) and row-group statistics pushdown
/// ([`Self::select_row_groups`]).
#[derive(Debug, Clone)]
pub struct BoundFilter {
    expr: BoundExpr,
    /// Referenced schema column indices, sorted + deduplicated.
    columns: Vec<usize>,
}

impl BoundFilter {
    /// Bind `expr` against `schema`. `renames` is the reserved-column rename
    /// list from `resolve_reserved_column_collisions` (#288), as `(old, new)`
    /// pairs: a filter column matching an OLD (input-file) name resolves to
    /// the renamed schema column, while pushdown keeps addressing the parquet
    /// footer by the old name.
    pub fn bind(
        expr: &FilterExpr,
        schema: &Schema,
        renames: &[(String, String)],
    ) -> Result<Self, FilterError> {
        let mut columns = Vec::new();
        let bound = bind_expr(expr, schema, renames, &mut columns)?;
        columns.sort_unstable();
        columns.dedup();
        Ok(BoundFilter {
            expr: bound,
            columns,
        })
    }

    /// Schema column indices the filter reads (for pass-1 projection).
    pub fn columns(&self) -> &[usize] {
        &self.columns
    }

    /// Evaluate the filter over `batch`, producing one tri-state result per
    /// row (`Some(true)` keep / `Some(false)` drop / `None` unknown — dropped
    /// at the top level per SQL semantics). `proj` maps a schema column index
    /// to the batch's column index (identity when the batch carries the full
    /// schema; the pass-1 projection mapping otherwise).
    pub fn eval_mask(
        &self,
        batch: &RecordBatch,
        proj: &dyn Fn(usize) -> usize,
    ) -> Vec<Option<bool>> {
        eval_expr(&self.expr, batch, proj)
    }

    /// Row groups of `metadata` the filter could match, by column chunk
    /// statistics — the pruning complement is proven row-group-free of
    /// matches. Conservative: missing statistics keep the row group.
    pub fn select_row_groups(&self, metadata: &ParquetMetaData) -> Vec<usize> {
        // Map top-level primitive parquet column name → leaf chunk index.
        let mut chunk_idx: HashMap<&str, usize> = HashMap::new();
        for (i, col) in metadata
            .file_metadata()
            .schema_descr()
            .columns()
            .iter()
            .enumerate()
        {
            let parts = col.path().parts();
            if parts.len() == 1 {
                chunk_idx.insert(parts[0].as_str(), i);
            }
        }
        (0..metadata.num_row_groups())
            .filter(|&i| rg_can_match(&self.expr, metadata.row_group(i), &chunk_idx))
            .collect()
    }
}

fn bind_expr(
    expr: &FilterExpr,
    schema: &Schema,
    renames: &[(String, String)],
    columns: &mut Vec<usize>,
) -> Result<BoundExpr, FilterError> {
    let bind_col = |name: &str, columns: &mut Vec<usize>| -> Result<BoundCol, FilterError> {
        // A filter naming a reserved-renamed input column (#288) follows the
        // rename for schema resolution; pushdown keeps the file-side name.
        let (schema_name, parquet_name) = match renames
            .iter()
            .find(|(old, _)| name.eq_ignore_ascii_case(old))
        {
            Some((old, new)) => (new.clone(), old.clone()),
            None => (name.to_string(), name.to_string()),
        };
        // Exact match first, then a unique case-insensitive fallback.
        let idx = schema.index_of(&schema_name).ok().or_else(|| {
            let mut found = None;
            for (i, f) in schema.fields().iter().enumerate() {
                if f.name().eq_ignore_ascii_case(&schema_name) {
                    if found.is_some() {
                        return None; // ambiguous
                    }
                    found = Some(i);
                }
            }
            found
        });
        let Some(idx) = idx else {
            let available = schema
                .fields()
                .iter()
                .map(|f| f.name().as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(FilterError::UnknownColumn {
                name: name.to_string(),
                available,
            });
        };
        let dt = schema.field(idx).data_type();
        let Some(kind) = column_kind(dt) else {
            return Err(FilterError::UnsupportedColumnType {
                name: name.to_string(),
                data_type: format!("{dt}"),
            });
        };
        columns.push(idx);
        Ok(BoundCol {
            idx,
            parquet_name,
            kind,
        })
    };

    // Type-check a literal against its column, coercing datetime strings
    // compared to timestamp columns into `Literal::Timestamp` epoch nanos
    // (so a malformed date errors here, at bind time, not per batch).
    let coerce_literal = |col: &BoundCol, lit: &Literal| -> Result<Literal, FilterError> {
        match (col.kind, lit) {
            (ColKind::Num, Literal::Number(_))
            | (ColKind::Str, Literal::String(_))
            | (ColKind::Bool, Literal::Bool(_)) => Ok(lit.clone()),
            (ColKind::Ts(_), Literal::String(s)) => {
                match arrow_cast::parse::string_to_timestamp_nanos(s) {
                    Ok(ns) => Ok(Literal::Timestamp(ns)),
                    Err(e) => Err(FilterError::TimestampLiteral {
                        name: col.parquet_name.clone(),
                        literal: s.clone(),
                        msg: e.to_string(),
                    }),
                }
            }
            _ => Err(FilterError::TypeMismatch {
                name: col.parquet_name.clone(),
                kind: col.kind.name(),
                literal: lit.describe(),
            }),
        }
    };

    match expr {
        FilterExpr::Compare { column, op, value } => {
            let col = bind_col(column, columns)?;
            let value = coerce_literal(&col, value)?;
            if col.kind == ColKind::Bool && !matches!(op, CmpOp::Eq | CmpOp::Ne) {
                return Err(FilterError::BooleanOrdering {
                    name: col.parquet_name,
                });
            }
            Ok(BoundExpr::Compare {
                col,
                op: *op,
                value,
            })
        }
        FilterExpr::In {
            column,
            values,
            negated,
        } => {
            let col = bind_col(column, columns)?;
            let values = values
                .iter()
                .map(|v| coerce_literal(&col, v))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(BoundExpr::In {
                col,
                values,
                negated: *negated,
            })
        }
        FilterExpr::IsNull { column, negated } => {
            let col = bind_col(column, columns)?;
            Ok(BoundExpr::IsNull {
                col,
                negated: *negated,
            })
        }
        FilterExpr::Not(inner) => Ok(BoundExpr::Not(Box::new(bind_expr(
            inner, schema, renames, columns,
        )?))),
        FilterExpr::And(a, b) => Ok(BoundExpr::And(
            Box::new(bind_expr(a, schema, renames, columns)?),
            Box::new(bind_expr(b, schema, renames, columns)?),
        )),
        FilterExpr::Or(a, b) => Ok(BoundExpr::Or(
            Box::new(bind_expr(a, schema, renames, columns)?),
            Box::new(bind_expr(b, schema, renames, columns)?),
        )),
    }
}

// ---------------------------------------------------------------------------
// Evaluation (per-batch, three-valued)
// ---------------------------------------------------------------------------

fn cmp_num(v: f64, op: CmpOp, lit: f64) -> bool {
    match op {
        CmpOp::Eq => v == lit,
        CmpOp::Ne => v != lit,
        CmpOp::Lt => v < lit,
        CmpOp::Le => v <= lit,
        CmpOp::Gt => v > lit,
        CmpOp::Ge => v >= lit,
    }
}

fn cmp_ord<T: Ord + ?Sized>(v: &T, op: CmpOp, lit: &T) -> bool {
    match op {
        CmpOp::Eq => v == lit,
        CmpOp::Ne => v != lit,
        CmpOp::Lt => v < lit,
        CmpOp::Le => v <= lit,
        CmpOp::Gt => v > lit,
        CmpOp::Ge => v >= lit,
    }
}

/// Apply `f` to each row of a string-kinded column (Utf8 / LargeUtf8 /
/// Utf8View / dictionary-of-string), producing `None` for null rows.
fn str_mask(col: &dyn Array, f: &dyn Fn(&str) -> bool) -> Vec<Option<bool>> {
    let n = col.len();
    match col.data_type() {
        DataType::Utf8 => {
            let a = col.as_string::<i32>();
            (0..n)
                .map(|i| (!a.is_null(i)).then(|| f(a.value(i))))
                .collect()
        }
        DataType::LargeUtf8 => {
            let a = col.as_string::<i64>();
            (0..n)
                .map(|i| (!a.is_null(i)).then(|| f(a.value(i))))
                .collect()
        }
        DataType::Utf8View => {
            let a = col.as_string_view();
            (0..n)
                .map(|i| (!a.is_null(i)).then(|| f(a.value(i))))
                .collect()
        }
        DataType::Dictionary(_, _) => {
            let Some(d) = col.as_any_dictionary_opt() else {
                return vec![None; n];
            };
            let values = d.values();
            let keys = d.normalized_keys();
            (0..n)
                .map(|i| {
                    if col.is_null(i) {
                        return None;
                    }
                    let k = keys[i];
                    if values.is_null(k) {
                        return None;
                    }
                    let s = match values.data_type() {
                        DataType::Utf8 => values.as_string::<i32>().value(k),
                        DataType::LargeUtf8 => values.as_string::<i64>().value(k),
                        DataType::Utf8View => values.as_string_view().value(k),
                        _ => return None,
                    };
                    Some(f(s))
                })
                .collect()
        }
        _ => vec![None; n],
    }
}

/// Per-row raw `i64` values of a timestamp column (any unit), `None` for
/// nulls or a non-timestamp array (unreachable after bind-time checking).
fn ts_values(col: &dyn Array) -> Vec<Option<i64>> {
    use arrow_array::types::{
        TimestampMicrosecondType, TimestampMillisecondType, TimestampNanosecondType,
        TimestampSecondType,
    };
    let n = col.len();
    fn read<T: arrow_array::ArrowPrimitiveType<Native = i64>>(
        col: &dyn Array,
        n: usize,
    ) -> Vec<Option<i64>> {
        let a = col.as_primitive::<T>();
        (0..n)
            .map(|i| (!a.is_null(i)).then(|| a.value(i)))
            .collect()
    }
    match col.data_type() {
        DataType::Timestamp(TimeUnit::Second, _) => read::<TimestampSecondType>(col, n),
        DataType::Timestamp(TimeUnit::Millisecond, _) => read::<TimestampMillisecondType>(col, n),
        DataType::Timestamp(TimeUnit::Microsecond, _) => read::<TimestampMicrosecondType>(col, n),
        DataType::Timestamp(TimeUnit::Nanosecond, _) => read::<TimestampNanosecondType>(col, n),
        _ => vec![None; n],
    }
}

fn eval_expr(
    expr: &BoundExpr,
    batch: &RecordBatch,
    proj: &dyn Fn(usize) -> usize,
) -> Vec<Option<bool>> {
    match expr {
        BoundExpr::Compare { col, op, value } => {
            let arr = batch.column(proj(col.idx));
            match (col.kind, value) {
                (ColKind::Num, Literal::Number(lit)) => extract_sort_keys(arr.as_ref())
                    .into_iter()
                    .map(|v| v.map(|v| cmp_num(v, *op, *lit)))
                    .collect(),
                (ColKind::Str, Literal::String(lit)) => {
                    str_mask(arr.as_ref(), &|s| cmp_ord(s, *op, lit.as_str()))
                }
                (ColKind::Bool, Literal::Bool(lit)) => {
                    let a = arr.as_boolean();
                    (0..a.len())
                        .map(|i| (!a.is_null(i)).then(|| cmp_ord(&a.value(i), *op, lit)))
                        .collect()
                }
                (ColKind::Ts(unit), Literal::Timestamp(lit)) => {
                    let f = ts_factor(unit);
                    let lit = *lit as i128;
                    ts_values(arr.as_ref())
                        .into_iter()
                        .map(|v| v.map(|v| cmp_ord(&(v as i128 * f), *op, &lit)))
                        .collect()
                }
                // Bind-time type checking makes this unreachable.
                _ => vec![None; batch.num_rows()],
            }
        }
        BoundExpr::In {
            col,
            values,
            negated,
        } => {
            let arr = batch.column(proj(col.idx));
            let member: Vec<Option<bool>> = match col.kind {
                ColKind::Num => {
                    let lits: Vec<f64> = values
                        .iter()
                        .filter_map(|l| match l {
                            Literal::Number(v) => Some(*v),
                            _ => None,
                        })
                        .collect();
                    extract_sort_keys(arr.as_ref())
                        .into_iter()
                        .map(|v| v.map(|v| lits.contains(&v)))
                        .collect()
                }
                ColKind::Str => {
                    let lits: Vec<&str> = values
                        .iter()
                        .filter_map(|l| match l {
                            Literal::String(s) => Some(s.as_str()),
                            _ => None,
                        })
                        .collect();
                    str_mask(arr.as_ref(), &|s| lits.contains(&s))
                }
                ColKind::Bool => {
                    let lits: Vec<bool> = values
                        .iter()
                        .filter_map(|l| match l {
                            Literal::Bool(b) => Some(*b),
                            _ => None,
                        })
                        .collect();
                    let a = arr.as_boolean();
                    (0..a.len())
                        .map(|i| (!a.is_null(i)).then(|| lits.contains(&a.value(i))))
                        .collect()
                }
                ColKind::Ts(unit) => {
                    let f = ts_factor(unit);
                    let lits: Vec<i128> = values
                        .iter()
                        .filter_map(|l| match l {
                            Literal::Timestamp(ns) => Some(*ns as i128),
                            _ => None,
                        })
                        .collect();
                    ts_values(arr.as_ref())
                        .into_iter()
                        .map(|v| v.map(|v| lits.contains(&(v as i128 * f))))
                        .collect()
                }
            };
            if *negated {
                member.into_iter().map(|m| m.map(|b| !b)).collect()
            } else {
                member
            }
        }
        BoundExpr::IsNull { col, negated } => {
            let arr = batch.column(proj(col.idx));
            (0..arr.len())
                .map(|i| Some(arr.is_null(i) != *negated))
                .collect()
        }
        // Kleene: NOT UNKNOWN = UNKNOWN.
        BoundExpr::Not(inner) => eval_expr(inner, batch, proj)
            .into_iter()
            .map(|v| v.map(|b| !b))
            .collect(),
        BoundExpr::And(a, b) => {
            let va = eval_expr(a, batch, proj);
            let vb = eval_expr(b, batch, proj);
            va.into_iter()
                .zip(vb)
                .map(|(x, y)| match (x, y) {
                    (Some(false), _) | (_, Some(false)) => Some(false),
                    (Some(true), Some(true)) => Some(true),
                    _ => None,
                })
                .collect()
        }
        BoundExpr::Or(a, b) => {
            let va = eval_expr(a, batch, proj);
            let vb = eval_expr(b, batch, proj);
            va.into_iter()
                .zip(vb)
                .map(|(x, y)| match (x, y) {
                    (Some(true), _) | (_, Some(true)) => Some(true),
                    (Some(false), Some(false)) => Some(false),
                    _ => None,
                })
                .collect()
        }
    }
}

// ---------------------------------------------------------------------------
// Row-group statistics pushdown
// ---------------------------------------------------------------------------

/// Widen an `i64` statistic to a conservative `f64` bound: outside the exact
/// range (|v| >= 2^53) rounding-to-nearest could tighten the interval, so the
/// bound is pushed outward by 2 ulp-equivalents before pruning.
fn widen_i64(v: i64, is_min: bool) -> f64 {
    const EXACT: i64 = 1 << 53;
    let f = v as f64;
    if v.abs() < EXACT {
        f
    } else if is_min {
        f - 2.0 * f.abs() * f64::EPSILON
    } else {
        f + 2.0 * f.abs() * f64::EPSILON
    }
}

/// `(min, max)` of a numeric column chunk as `f64` bounds, when available.
fn num_bounds(stats: &Statistics) -> Option<(f64, f64)> {
    match stats {
        Statistics::Int32(s) => Some((*s.min_opt()? as f64, *s.max_opt()? as f64)),
        Statistics::Int64(s) => Some((
            widen_i64(*s.min_opt()?, true),
            widen_i64(*s.max_opt()?, false),
        )),
        Statistics::Float(s) => Some((*s.min_opt()? as f64, *s.max_opt()? as f64)),
        Statistics::Double(s) => Some((*s.min_opt()?, *s.max_opt()?)),
        _ => None,
    }
}

/// `(min, max)` of a string column chunk, when available and valid UTF-8.
/// Writer truncation keeps these valid bounds (a truncated max is adjusted
/// upward), so bound-based pruning stays conservative.
fn str_bounds(stats: &Statistics) -> Option<(&str, &str)> {
    match stats {
        Statistics::ByteArray(s) => Some((
            std::str::from_utf8(s.min_opt()?.data()).ok()?,
            std::str::from_utf8(s.max_opt()?.data()).ok()?,
        )),
        _ => None,
    }
}

fn bool_bounds(stats: &Statistics) -> Option<(bool, bool)> {
    match stats {
        Statistics::Boolean(s) => Some((*s.min_opt()?, *s.max_opt()?)),
        _ => None,
    }
}

/// Could `min <= v <= max` hold for some v satisfying `v op lit`?
fn range_can_match_num(min: f64, max: f64, op: CmpOp, lit: f64) -> bool {
    match op {
        CmpOp::Eq => min <= lit && lit <= max,
        CmpOp::Ne => !(min == max && min == lit),
        CmpOp::Lt => min < lit,
        CmpOp::Le => min <= lit,
        CmpOp::Gt => max > lit,
        CmpOp::Ge => max >= lit,
    }
}

fn range_can_match_ord<T: Ord + ?Sized>(min: &T, max: &T, op: CmpOp, lit: &T) -> bool {
    match op {
        CmpOp::Eq => min <= lit && lit <= max,
        CmpOp::Ne => !(min == max && min == lit),
        CmpOp::Lt => min < lit,
        CmpOp::Le => min <= lit,
        CmpOp::Gt => max > lit,
        CmpOp::Ge => max >= lit,
    }
}

/// Whether the row group could contain a row matching `expr`, judged by
/// column chunk statistics alone. `true` = must read; `false` = provably no
/// match (prune). Conservative in every uncertain case.
fn rg_can_match(expr: &BoundExpr, rg: &RowGroupMetaData, chunk_idx: &HashMap<&str, usize>) -> bool {
    // Statistics for a bound column's chunk, if locatable.
    let stats_for = |col: &BoundCol| -> Option<&Statistics> {
        let i = *chunk_idx.get(col.parquet_name.as_str())?;
        rg.column(i).statistics()
    };
    // A comparison / IN only matches non-null values: an all-null chunk can
    // never satisfy one.
    let all_null = |col: &BoundCol| -> bool {
        stats_for(col)
            .and_then(|s| s.null_count_opt())
            .is_some_and(|nc| nc == rg.num_rows() as u64)
    };
    let value_can_match = |col: &BoundCol, op: CmpOp, lit: &Literal| -> bool {
        let Some(stats) = stats_for(col) else {
            return true;
        };
        match lit {
            Literal::Number(l) => match num_bounds(stats) {
                Some((min, max)) => range_can_match_num(min, max, op, *l),
                None => true,
            },
            Literal::String(l) => match str_bounds(stats) {
                Some((min, max)) => range_can_match_ord(min, max, op, l.as_str()),
                None => true,
            },
            Literal::Bool(l) => match bool_bounds(stats) {
                Some((min, max)) => range_can_match_ord(&min, &max, op, l),
                None => true,
            },
            // Timestamp stats are physical Int64 in the column's own unit
            // (INT96 legacy chunks expose no stats and stay conservative);
            // widen both sides to i128 epoch nanos for an exact comparison.
            Literal::Timestamp(l) => match (col.kind, stats) {
                (ColKind::Ts(unit), Statistics::Int64(s)) => match (s.min_opt(), s.max_opt()) {
                    (Some(min), Some(max)) => {
                        let f = ts_factor(unit);
                        range_can_match_ord(
                            &(*min as i128 * f),
                            &(*max as i128 * f),
                            op,
                            &(*l as i128),
                        )
                    }
                    _ => true,
                },
                _ => true,
            },
        }
    };

    match expr {
        BoundExpr::Compare { col, op, value } => !all_null(col) && value_can_match(col, *op, value),
        BoundExpr::In {
            col,
            values,
            negated,
        } => {
            if all_null(col) {
                return false;
            }
            if *negated {
                // NOT IN can only be pruned when the chunk is constant AND
                // that constant is in the list.
                !values.iter().all(|v| {
                    // v constant-excludes the chunk iff Ne can't match.
                    !value_can_match(col, CmpOp::Ne, v)
                })
            } else {
                values.iter().any(|v| value_can_match(col, CmpOp::Eq, v))
            }
        }
        BoundExpr::IsNull { col, negated } => {
            let Some(stats) = stats_for(col) else {
                return true;
            };
            let Some(nc) = stats.null_count_opt() else {
                return true;
            };
            if *negated {
                // IS NOT NULL: prune only an all-null chunk.
                nc < rg.num_rows() as u64
            } else {
                // IS NULL: prune a null-free chunk.
                nc > 0
            }
        }
        // `NOT (subtree)` cannot be inverted from a can-match bound
        // conservatively — keep the row group; per-row eval decides.
        BoundExpr::Not(_) => true,
        BoundExpr::And(a, b) => rg_can_match(a, rg, chunk_idx) && rg_can_match(b, rg, chunk_idx),
        BoundExpr::Or(a, b) => rg_can_match(a, rg, chunk_idx) || rg_can_match(b, rg, chunk_idx),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{
        ArrayRef, BooleanArray, Float64Array, Int64Array, StringArray, TimestampMicrosecondArray,
        TimestampMillisecondArray, TimestampNanosecondArray, TimestampSecondArray,
    };
    use arrow_schema::{Field, TimeUnit};
    use std::sync::Arc;

    // ---- parser ----------------------------------------------------------

    #[test]
    fn parses_simple_comparison() {
        let e = parse_filter("confidence > 0.8").unwrap();
        assert_eq!(
            e,
            FilterExpr::Compare {
                column: "confidence".into(),
                op: CmpOp::Gt,
                value: Literal::Number(0.8),
            }
        );
    }

    #[test]
    fn parses_all_comparison_operators() {
        for (src, op) in [
            ("x = 1", CmpOp::Eq),
            ("x == 1", CmpOp::Eq),
            ("x != 1", CmpOp::Ne),
            ("x <> 1", CmpOp::Ne),
            ("x < 1", CmpOp::Lt),
            ("x <= 1", CmpOp::Le),
            ("x > 1", CmpOp::Gt),
            ("x >= 1", CmpOp::Ge),
        ] {
            let e = parse_filter(src).unwrap();
            assert_eq!(
                e,
                FilterExpr::Compare {
                    column: "x".into(),
                    op,
                    value: Literal::Number(1.0),
                },
                "source {src:?}"
            );
        }
    }

    #[test]
    fn parses_string_and_bool_and_negative_literals() {
        assert_eq!(
            parse_filter("name = 'it''s'").unwrap(),
            FilterExpr::Compare {
                column: "name".into(),
                op: CmpOp::Eq,
                value: Literal::String("it's".into()),
            }
        );
        assert_eq!(
            parse_filter("active = true").unwrap(),
            FilterExpr::Compare {
                column: "active".into(),
                op: CmpOp::Eq,
                value: Literal::Bool(true),
            }
        );
        assert_eq!(
            parse_filter("t <= -2.5e3").unwrap(),
            FilterExpr::Compare {
                column: "t".into(),
                op: CmpOp::Le,
                value: Literal::Number(-2500.0),
            }
        );
    }

    #[test]
    fn parses_in_not_in_is_null() {
        assert_eq!(
            parse_filter("crop IN ('soy', 'corn')").unwrap(),
            FilterExpr::In {
                column: "crop".into(),
                values: vec![
                    Literal::String("soy".into()),
                    Literal::String("corn".into())
                ],
                negated: false,
            }
        );
        assert_eq!(
            parse_filter("id not in (1, 2, 3)").unwrap(),
            FilterExpr::In {
                column: "id".into(),
                values: vec![
                    Literal::Number(1.0),
                    Literal::Number(2.0),
                    Literal::Number(3.0)
                ],
                negated: true,
            }
        );
        assert_eq!(
            parse_filter("note IS NULL").unwrap(),
            FilterExpr::IsNull {
                column: "note".into(),
                negated: false,
            }
        );
        assert_eq!(
            parse_filter("note is not null").unwrap(),
            FilterExpr::IsNull {
                column: "note".into(),
                negated: true,
            }
        );
    }

    #[test]
    fn precedence_or_below_and_below_not() {
        // a = 1 OR b = 2 AND NOT c = 3  ==  a=1 OR (b=2 AND (NOT c=3))
        let e = parse_filter("a = 1 OR b = 2 AND NOT c = 3").unwrap();
        let FilterExpr::Or(left, right) = e else {
            panic!("top must be OR, got {e:?}");
        };
        assert!(matches!(*left, FilterExpr::Compare { .. }));
        let FilterExpr::And(al, ar) = *right else {
            panic!("right of OR must be AND");
        };
        assert!(matches!(*al, FilterExpr::Compare { .. }));
        assert!(matches!(*ar, FilterExpr::Not(_)));
    }

    #[test]
    fn parses_parentheses_and_quoted_identifiers() {
        let e = parse_filter("(a = 1 OR b = 2) AND \"weird col\" >= 7").unwrap();
        let FilterExpr::And(l, r) = e else {
            panic!("top must be AND");
        };
        assert!(matches!(*l, FilterExpr::Or(_, _)));
        assert_eq!(
            *r,
            FilterExpr::Compare {
                column: "weird col".into(),
                op: CmpOp::Ge,
                value: Literal::Number(7.0),
            }
        );
    }

    #[test]
    fn parse_errors() {
        for bad in [
            "",
            "confidence >",
            "confidence > > 1",
            "x = NULL",
            "IN (1)",
            "x IN ()",
            "x IN (1",
            "(a = 1",
            "a = 'unterminated",
            "a = 1 extra",
            "a ! 1",
        ] {
            assert!(
                matches!(parse_filter(bad), Err(FilterError::Parse(_))),
                "expected parse error for {bad:?}"
            );
        }
    }

    #[test]
    fn null_literal_comparison_hints_is_null() {
        let err = parse_filter("x = NULL").unwrap_err();
        assert!(err.to_string().contains("IS NULL"), "got: {err}");
    }

    // ---- bind + eval -----------------------------------------------------

    fn test_batch() -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("confidence", DataType::Float64, true),
            Field::new("crop", DataType::Utf8, true),
            Field::new("id", DataType::Int64, false),
            Field::new("active", DataType::Boolean, true),
        ]));
        let confidence: ArrayRef = Arc::new(Float64Array::from(vec![
            Some(0.9),
            Some(0.5),
            None,
            Some(0.85),
        ]));
        let crop: ArrayRef = Arc::new(StringArray::from(vec![
            Some("soy"),
            Some("corn"),
            Some("soy"),
            None,
        ]));
        let id: ArrayRef = Arc::new(Int64Array::from(vec![0, 1, 2, 3]));
        let active: ArrayRef = Arc::new(BooleanArray::from(vec![
            Some(true),
            Some(false),
            None,
            Some(true),
        ]));
        RecordBatch::try_new(schema, vec![confidence, crop, id, active]).unwrap()
    }

    fn eval(src: &str) -> Vec<Option<bool>> {
        let batch = test_batch();
        let expr = parse_filter(src).unwrap();
        let bound = BoundFilter::bind(&expr, &batch.schema(), &[]).unwrap();
        bound.eval_mask(&batch, &|i| i)
    }

    #[test]
    fn eval_numeric_comparison_with_nulls() {
        // Row 2 has null confidence → UNKNOWN, dropped at the top level.
        assert_eq!(
            eval("confidence > 0.8"),
            vec![Some(true), Some(false), None, Some(true)]
        );
    }

    #[test]
    fn eval_three_valued_not_and_or() {
        // NOT propagates UNKNOWN (row 2 stays None, is not resurrected).
        assert_eq!(
            eval("NOT confidence > 0.8"),
            vec![Some(false), Some(true), None, Some(false)]
        );
        // UNKNOWN AND FALSE = FALSE; UNKNOWN AND TRUE = UNKNOWN.
        assert_eq!(
            eval("confidence > 0.8 AND crop = 'soy'"),
            vec![Some(true), Some(false), None, None]
        );
        // UNKNOWN OR TRUE = TRUE.
        assert_eq!(
            eval("confidence > 0.8 OR crop = 'soy'"),
            vec![Some(true), Some(false), Some(true), Some(true)]
        );
    }

    #[test]
    fn eval_in_and_is_null() {
        assert_eq!(
            eval("crop IN ('soy', 'wheat')"),
            vec![Some(true), Some(false), Some(true), None]
        );
        assert_eq!(
            eval("crop NOT IN ('soy')"),
            vec![Some(false), Some(true), Some(false), None]
        );
        assert_eq!(
            eval("confidence IS NULL"),
            vec![Some(false), Some(false), Some(true), Some(false)]
        );
        assert_eq!(
            eval("crop IS NOT NULL"),
            vec![Some(true), Some(true), Some(true), Some(false)]
        );
        assert_eq!(
            eval("id IN (1, 3)"),
            vec![Some(false), Some(true), Some(false), Some(true)]
        );
    }

    #[test]
    fn eval_bool_and_string_ordering() {
        assert_eq!(
            eval("active = true"),
            vec![Some(true), Some(false), None, Some(true)]
        );
        // Lexicographic string ordering.
        assert_eq!(
            eval("crop >= 'soy'"),
            vec![Some(true), Some(false), Some(true), None]
        );
    }

    #[test]
    fn bind_errors() {
        let batch = test_batch();
        let schema = batch.schema();
        let unknown = parse_filter("nope = 1").unwrap();
        assert!(matches!(
            BoundFilter::bind(&unknown, &schema, &[]),
            Err(FilterError::UnknownColumn { .. })
        ));
        let mismatch = parse_filter("crop > 3").unwrap();
        assert!(matches!(
            BoundFilter::bind(&mismatch, &schema, &[]),
            Err(FilterError::TypeMismatch { .. })
        ));
        let bool_ord = parse_filter("active > false").unwrap();
        assert!(matches!(
            BoundFilter::bind(&bool_ord, &schema, &[]),
            Err(FilterError::BooleanOrdering { .. })
        ));
    }

    #[test]
    fn bind_follows_reserved_renames() {
        // Input column `level` renamed to `level_` (#288): the filter's
        // `level` resolves to the renamed schema column, pushdown keeps the
        // parquet-side name `level`.
        let schema = Schema::new(vec![
            Field::new("level_", DataType::Int64, true),
            Field::new("geometry", DataType::Binary, true),
        ]);
        let expr = parse_filter("level >= 2").unwrap();
        let bound = BoundFilter::bind(
            &expr,
            &schema,
            &[("level".to_string(), "level_".to_string())],
        )
        .unwrap();
        assert_eq!(bound.columns(), &[0]);
        let BoundExpr::Compare { col, .. } = &bound.expr else {
            panic!("compare expected");
        };
        assert_eq!(col.parquet_name, "level");
        assert_eq!(col.idx, 0);
    }

    #[test]
    fn bind_case_insensitive_fallback() {
        let batch = test_batch();
        let expr = parse_filter("CONFIDENCE > 0.8").unwrap();
        let bound = BoundFilter::bind(&expr, &batch.schema(), &[]).unwrap();
        assert_eq!(bound.columns(), &[0]);
    }

    // ---- pushdown --------------------------------------------------------

    /// Write a parquet file with one row group per (confidence, crop) pair
    /// and return its metadata.
    fn stats_file(
        rows: &[(Option<f64>, Option<&str>)],
    ) -> (tempfile::NamedTempFile, ParquetMetaData) {
        use parquet::arrow::ArrowWriter;
        use parquet::file::properties::WriterProperties;

        let schema = Arc::new(Schema::new(vec![
            Field::new("confidence", DataType::Float64, true),
            Field::new("crop", DataType::Utf8, true),
        ]));
        let file = tempfile::NamedTempFile::new().unwrap();
        let props = WriterProperties::builder()
            .set_max_row_group_row_count(Some(1))
            .build();
        let mut writer =
            ArrowWriter::try_new(file.reopen().unwrap(), schema.clone(), Some(props)).unwrap();
        let conf: ArrayRef = Arc::new(Float64Array::from(
            rows.iter().map(|(c, _)| *c).collect::<Vec<_>>(),
        ));
        let crop: ArrayRef = Arc::new(StringArray::from(
            rows.iter().map(|(_, s)| *s).collect::<Vec<_>>(),
        ));
        let batch = RecordBatch::try_new(schema, vec![conf, crop]).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();

        let f = std::fs::File::open(file.path()).unwrap();
        let reader = parquet::file::reader::SerializedFileReader::new(f).unwrap();
        use parquet::file::reader::FileReader;
        let meta = reader.metadata().clone();
        (file, meta)
    }

    fn selected(src: &str, meta: &ParquetMetaData) -> Vec<usize> {
        let schema = Schema::new(vec![
            Field::new("confidence", DataType::Float64, true),
            Field::new("crop", DataType::Utf8, true),
        ]);
        let expr = parse_filter(src).unwrap();
        let bound = BoundFilter::bind(&expr, &schema, &[]).unwrap();
        bound.select_row_groups(meta)
    }

    #[test]
    fn pushdown_prunes_by_numeric_min_max() {
        // 4 single-row row groups: confidence 0.1, 0.5, 0.9, null.
        let (_f, meta) = stats_file(&[
            (Some(0.1), Some("soy")),
            (Some(0.5), Some("corn")),
            (Some(0.9), Some("soy")),
            (None, Some("rice")),
        ]);
        assert_eq!(meta.num_row_groups(), 4);
        // > 0.8: only RG2 can match (RG3 is all-null → pruned).
        assert_eq!(selected("confidence > 0.8", &meta), vec![2]);
        // <= 0.5: RG0, RG1.
        assert_eq!(selected("confidence <= 0.5", &meta), vec![0, 1]);
        // = 0.5: RG1 only.
        assert_eq!(selected("confidence = 0.5", &meta), vec![1]);
        // IS NULL: RG3 only (others null-free).
        assert_eq!(selected("confidence IS NULL", &meta), vec![3]);
        // IS NOT NULL: all but the all-null RG3.
        assert_eq!(selected("confidence IS NOT NULL", &meta), vec![0, 1, 2]);
    }

    #[test]
    fn pushdown_prunes_strings_and_composes_and_or() {
        let (_f, meta) = stats_file(&[
            (Some(0.1), Some("soy")),
            (Some(0.5), Some("corn")),
            (Some(0.9), Some("soy")),
            (None, Some("rice")),
        ]);
        assert_eq!(selected("crop = 'soy'", &meta), vec![0, 2]);
        assert_eq!(selected("crop IN ('corn', 'rice')", &meta), vec![1, 3]);
        // AND intersects; OR unions.
        assert_eq!(
            selected("crop = 'soy' AND confidence > 0.8", &meta),
            vec![2]
        );
        assert_eq!(
            selected("crop = 'corn' OR confidence > 0.8", &meta),
            vec![1, 2]
        );
        // != prunes only a constant chunk equal to the literal (all
        // single-row chunks here are constant).
        assert_eq!(selected("crop != 'soy'", &meta), vec![1, 3]);
        // NOT (...) is conservative: keeps everything.
        assert_eq!(selected("NOT (crop = 'soy')", &meta), vec![0, 1, 2, 3]);
    }

    // ---- timestamps ------------------------------------------------------

    /// Epoch microseconds for 2024-01-01T00:00:00Z / 2025-01-01T00:00:00Z.
    const T2024_US: i64 = 1_704_067_200_000_000;
    const T2025_US: i64 = 1_735_689_600_000_000;

    fn ts_batch(unit: TimeUnit, tz: Option<&str>) -> RecordBatch {
        let dt = DataType::Timestamp(unit, tz.map(|s| s.into()));
        let schema = Arc::new(Schema::new(vec![
            Field::new("time", dt.clone(), true),
            Field::new("label", DataType::Utf8, true),
        ]));
        let div = match unit {
            TimeUnit::Second => 1_000_000,
            TimeUnit::Millisecond => 1_000,
            TimeUnit::Microsecond => 1,
            TimeUnit::Nanosecond => 1, // multiplied below instead
        };
        let mul = if unit == TimeUnit::Nanosecond {
            1_000
        } else {
            1
        };
        let vals: Vec<Option<i64>> = vec![
            Some(T2024_US / div * mul),
            Some(T2025_US / div * mul),
            None,
            Some(T2025_US / div * mul),
        ];
        let time: ArrayRef = match unit {
            TimeUnit::Second => Arc::new(TimestampSecondArray::from(vals).with_data_type(dt)),
            TimeUnit::Millisecond => {
                Arc::new(TimestampMillisecondArray::from(vals).with_data_type(dt))
            }
            TimeUnit::Microsecond => {
                Arc::new(TimestampMicrosecondArray::from(vals).with_data_type(dt))
            }
            TimeUnit::Nanosecond => {
                Arc::new(TimestampNanosecondArray::from(vals).with_data_type(dt))
            }
        };
        let label: ArrayRef = Arc::new(StringArray::from(vec![
            Some("field"),
            Some("field"),
            Some("other"),
            Some("other"),
        ]));
        RecordBatch::try_new(schema, vec![time, label]).unwrap()
    }

    fn eval_ts(src: &str, unit: TimeUnit, tz: Option<&str>) -> Vec<Option<bool>> {
        let batch = ts_batch(unit, tz);
        let expr = parse_filter(src).unwrap();
        let bound = BoundFilter::bind(&expr, &batch.schema(), &[]).unwrap();
        bound.eval_mask(&batch, &|i| i)
    }

    #[test]
    fn eval_timestamp_comparisons_all_units() {
        for unit in [
            TimeUnit::Second,
            TimeUnit::Millisecond,
            TimeUnit::Microsecond,
            TimeUnit::Nanosecond,
        ] {
            assert_eq!(
                eval_ts("time >= '2025-01-01'", unit, None),
                vec![Some(false), Some(true), None, Some(true)],
                "unit {unit:?}"
            );
            assert_eq!(
                eval_ts("time < '2025-01-01 00:00:00'", unit, None),
                vec![Some(true), Some(false), None, Some(false)],
                "unit {unit:?}"
            );
            assert_eq!(
                eval_ts("time = '2024-01-01T00:00:00Z'", unit, None),
                vec![Some(true), Some(false), None, Some(false)],
                "unit {unit:?}"
            );
        }
    }

    #[test]
    fn eval_timestamp_tz_column_and_composition() {
        // A tz-annotated column stores the same UTC epoch; literals are UTC.
        assert_eq!(
            eval_ts(
                "time >= '2025-01-01' AND label = 'field'",
                TimeUnit::Microsecond,
                Some("UTC"),
            ),
            // Row 2: UNKNOWN (null time) AND FALSE (label mismatch) = FALSE.
            vec![Some(false), Some(true), Some(false), Some(false)]
        );
        assert_eq!(
            eval_ts(
                "time IN ('2024-01-01', '2026-01-01')",
                TimeUnit::Microsecond,
                None,
            ),
            vec![Some(true), Some(false), None, Some(false)]
        );
        assert_eq!(
            eval_ts("time IS NULL", TimeUnit::Microsecond, None),
            vec![Some(false), Some(false), Some(true), Some(false)]
        );
    }

    #[test]
    fn bind_timestamp_errors() {
        let batch = ts_batch(TimeUnit::Microsecond, None);
        let schema = batch.schema();
        // A numeric literal against a timestamp column is a type mismatch.
        let num = parse_filter("time > 5").unwrap();
        assert!(matches!(
            BoundFilter::bind(&num, &schema, &[]),
            Err(FilterError::TypeMismatch { .. })
        ));
        // An unparseable datetime string errors at bind time, not eval time.
        let bad = parse_filter("time > 'not-a-date'").unwrap();
        assert!(BoundFilter::bind(&bad, &schema, &[]).is_err());
    }

    /// Parquet file with a Timestamp(micros) column, one row group per value.
    fn ts_stats_file(rows: &[Option<i64>]) -> (tempfile::NamedTempFile, ParquetMetaData) {
        use parquet::arrow::ArrowWriter;
        use parquet::file::properties::WriterProperties;

        let dt = DataType::Timestamp(TimeUnit::Microsecond, None);
        let schema = Arc::new(Schema::new(vec![Field::new("time", dt.clone(), true)]));
        let file = tempfile::NamedTempFile::new().unwrap();
        let props = WriterProperties::builder()
            .set_max_row_group_row_count(Some(1))
            .build();
        let mut writer =
            ArrowWriter::try_new(file.reopen().unwrap(), schema.clone(), Some(props)).unwrap();
        let time: ArrayRef =
            Arc::new(TimestampMicrosecondArray::from(rows.to_vec()).with_data_type(dt));
        let batch = RecordBatch::try_new(schema, vec![time]).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();

        let f = std::fs::File::open(file.path()).unwrap();
        let reader = parquet::file::reader::SerializedFileReader::new(f).unwrap();
        use parquet::file::reader::FileReader;
        let meta = reader.metadata().clone();
        (file, meta)
    }

    #[test]
    fn pushdown_prunes_by_timestamp_min_max() {
        let (_f, meta) = ts_stats_file(&[Some(T2024_US), Some(T2025_US), None]);
        assert_eq!(meta.num_row_groups(), 3);
        let schema = Schema::new(vec![Field::new(
            "time",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            true,
        )]);
        let sel = |src: &str| {
            let expr = parse_filter(src).unwrap();
            let bound = BoundFilter::bind(&expr, &schema, &[]).unwrap();
            bound.select_row_groups(&meta)
        };
        // >= 2025: the 2024 row group and the all-null one are pruned.
        assert_eq!(sel("time >= '2025-01-01'"), vec![1]);
        assert_eq!(sel("time < '2025-01-01'"), vec![0]);
        assert_eq!(sel("time IS NULL"), vec![2]);
    }

    #[test]
    fn widen_i64_is_conservative_beyond_2p53() {
        let big = (1i64 << 53) + 1;
        assert!(widen_i64(big, true) < big as f64);
        assert!(widen_i64(big, false) > big as f64);
        assert_eq!(widen_i64(42, true), 42.0);
        assert_eq!(widen_i64(-42, false), -42.0);
    }
}
