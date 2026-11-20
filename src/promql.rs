// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Safe, bounded PromQL subset parser and O(1) evaluator for Sashiko's
//! `/api/v1/query` and `/api/v1/query_range` Prometheus HTTP API endpoints.

use std::collections::BTreeMap;

use regex::RegexBuilder;
use serde::Serialize;

pub const MAX_QUERY_LEN: usize = 2048;
pub const MAX_AST_DEPTH: usize = 8;
pub const MAX_AST_NODES: usize = 64;
pub const MAX_MATCHERS_PER_SELECTOR: usize = 16;
pub const MAX_RANGE_POINTS: usize = 400;
pub const MIN_STEP_SECS: i64 = 60;
pub const MAX_DURATION_SECS: i64 = 10 * 365 * 86_400;
pub const MAX_TIMESTAMP_SECS: i64 = 10_000_000_000;
const REGEX_SIZE_LIMIT: usize = 64 * 1024;

pub type Labels = BTreeMap<String, String>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromError {
    BadData(String),
    Internal(String),
}

impl PromError {
    pub fn bad_data(msg: impl Into<String>) -> Self {
        Self::BadData(msg.into())
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    pub const fn error_type(&self) -> &'static str {
        match self {
            Self::BadData(_) => "bad_data",
            Self::Internal(_) => "internal",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::BadData(m) | Self::Internal(m) => m,
        }
    }
}

#[derive(Debug, Clone)]
pub enum LabelMatchOp {
    Eq(String),
    NotEq(String),
    RegexEq(regex::Regex),
    RegexNotEq(regex::Regex),
}

#[derive(Debug, Clone)]
pub struct LabelMatcher {
    pub name: String,
    pub op: LabelMatchOp,
}

impl LabelMatcher {
    pub fn matches(&self, labels: &Labels) -> bool {
        let val = labels.get(&self.name).map_or("", String::as_str);
        match &self.op {
            LabelMatchOp::Eq(exp) => val == exp,
            LabelMatchOp::NotEq(exp) => val != exp,
            LabelMatchOp::RegexEq(re) => re.is_match(val),
            LabelMatchOp::RegexNotEq(re) => !re.is_match(val),
        }
    }
}

#[derive(Debug, Clone)]
pub struct VectorSelector {
    pub metric: String,
    pub matchers: Vec<LabelMatcher>,
    pub range_secs: Option<i64>,
    pub offset_secs: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeFunc {
    Increase,
    Rate,
    SumOverTime,
    AvgOverTime,
    MaxOverTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggOp {
    Sum,
    Avg,
    Min,
    Max,
    Count,
}

#[derive(Debug, Clone)]
pub enum Grouping {
    All,
    By(Vec<String>),
    Without(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Number(f64),
    Selector(VectorSelector),
    RangeCall {
        func: RangeFunc,
        selector: VectorSelector,
    },
    Aggregate {
        op: AggOp,
        grouping: Grouping,
        expr: Box<Expr>,
    },
    TopK {
        k: usize,
        expr: Box<Expr>,
    },
    HistogramQuantile {
        phi: f64,
        expr: Box<Expr>,
    },
    ClampMin {
        expr: Box<Expr>,
        min: f64,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
}

// ============================================================================
// Wire Response Types (Prometheus HTTP API v1)
// ============================================================================

#[derive(Debug, Clone, Serialize)]
pub struct VectorSample {
    pub metric: Labels,
    pub value: (f64, String),
}

#[derive(Debug, Clone, Serialize)]
pub struct MatrixSeries {
    pub metric: Labels,
    pub values: Vec<(f64, String)>,
}

pub fn format_prom_float(v: f64) -> String {
    if v.is_nan() {
        "NaN".to_string()
    } else if v.is_infinite() {
        if v.is_sign_positive() {
            "+Inf".to_string()
        } else {
            "-Inf".to_string()
        }
    } else if v.fract() == 0.0 && v.abs() < 1e16 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

pub fn parse_prom_duration_secs(raw: &str) -> Result<i64, PromError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(PromError::bad_data("empty duration"));
    }
    if let Ok(secs) = s.parse::<f64>() {
        if !secs.is_finite() || secs <= 0.0 || secs > MAX_DURATION_SECS as f64 {
            return Err(PromError::bad_data(format!(
                "duration out of range (0..={MAX_DURATION_SECS}s): {s}"
            )));
        }
        return Ok((secs.round() as i64).clamp(1, MAX_DURATION_SECS));
    }
    let mut total: i64 = 0;
    let mut idx = 0;
    let bytes = s.as_bytes();
    while idx < bytes.len() {
        let num_start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == num_start {
            return Err(PromError::bad_data(format!("invalid duration: {s}")));
        }
        let n = s[num_start..idx]
            .parse::<i64>()
            .map_err(|_| PromError::bad_data(format!("duration overflow: {s}")))?;
        let unit_start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_alphabetic() {
            idx += 1;
        }
        let mult = match &s[unit_start..idx] {
            "s" => 1,
            "m" => 60,
            "h" => 3600,
            "d" => 86_400,
            "w" => 7 * 86_400,
            "y" => 365 * 86_400,
            u => {
                return Err(PromError::bad_data(format!(
                    "unsupported unit '{u}' in {s}"
                )));
            }
        };
        total = total
            .checked_add(
                n.checked_mul(mult)
                    .ok_or_else(|| PromError::bad_data("duration overflow"))?,
            )
            .ok_or_else(|| PromError::bad_data("duration overflow"))?;
    }
    if total <= 0 || total > MAX_DURATION_SECS {
        return Err(PromError::bad_data(format!(
            "duration out of range (1..={MAX_DURATION_SECS}s): {s}"
        )));
    }
    Ok(total)
}

pub fn parse_prom_timestamp(raw: &str, default_now: i64) -> Result<i64, PromError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(default_now.clamp(0, MAX_TIMESTAMP_SECS));
    }
    if let Ok(ts) = trimmed.parse::<f64>() {
        if !ts.is_finite() || !(0.0..=MAX_TIMESTAMP_SECS as f64).contains(&ts) {
            return Err(PromError::bad_data(format!(
                "timestamp out of range (0..={MAX_TIMESTAMP_SECS}): {trimmed}"
            )));
        }
        return Ok((ts.floor() as i64).clamp(0, MAX_TIMESTAMP_SECS));
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        let ts = dt.timestamp();
        if !(0..=MAX_TIMESTAMP_SECS).contains(&ts) {
            return Err(PromError::bad_data(format!(
                "timestamp out of range (0..={MAX_TIMESTAMP_SECS}): {trimmed}"
            )));
        }
        return Ok(ts);
    }
    Err(PromError::bad_data(format!("invalid timestamp: {trimmed}")))
}

// ============================================================================
// Recursive-Descent PromQL Subset Parser
// ============================================================================

pub fn parse_promql(input: &str) -> Result<Expr, PromError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(PromError::bad_data("query cannot be empty"));
    }
    if trimmed.len() > MAX_QUERY_LEN {
        return Err(PromError::bad_data("query exceeds maximum length"));
    }
    let mut p = Parser {
        s: trimmed,
        pos: 0,
        nodes: 0,
    };
    let expr = p.parse_expr(0)?;
    p.skip_ws();
    if p.pos != p.s.len() {
        return Err(PromError::bad_data(format!(
            "unexpected trailing input near '{}'",
            &p.s[p.pos..]
        )));
    }
    Ok(expr)
}

struct Parser<'a> {
    s: &'a str,
    pos: usize,
    nodes: usize,
}

impl<'a> Parser<'a> {
    fn bump_node(&mut self) -> Result<(), PromError> {
        self.nodes += 1;
        if self.nodes > MAX_AST_NODES {
            return Err(PromError::bad_data(format!(
                "expression exceeds maximum {MAX_AST_NODES} AST nodes"
            )));
        }
        Ok(())
    }
    fn skip_ws(&mut self) {
        while let Some(b) = self.s.as_bytes().get(self.pos) {
            if b.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek_byte(&self) -> Option<u8> {
        self.s.as_bytes().get(self.pos).copied()
    }

    fn eat_char(&mut self, expected: u8) -> bool {
        self.skip_ws();
        if self.peek_byte() == Some(expected) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn parse_expr(&mut self, depth: usize) -> Result<Expr, PromError> {
        if depth > MAX_AST_DEPTH {
            return Err(PromError::bad_data("expression nesting too deep"));
        }
        self.parse_add_sub(depth)
    }

    fn parse_add_sub(&mut self, depth: usize) -> Result<Expr, PromError> {
        let mut lhs = self.parse_mul_div(depth)?;
        loop {
            self.skip_ws();
            let op = match self.peek_byte() {
                Some(b'+') => BinOp::Add,
                Some(b'-') => BinOp::Sub,
                _ => break,
            };
            self.pos += 1;
            self.bump_node()?;
            let rhs = self.parse_mul_div(depth + 1)?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_mul_div(&mut self, depth: usize) -> Result<Expr, PromError> {
        let mut lhs = self.parse_primary(depth)?;
        loop {
            self.skip_ws();
            let op = match self.peek_byte() {
                Some(b'*') => BinOp::Mul,
                Some(b'/') => BinOp::Div,
                _ => break,
            };
            self.pos += 1;
            self.bump_node()?;
            let rhs = self.parse_primary(depth + 1)?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_primary(&mut self, depth: usize) -> Result<Expr, PromError> {
        self.bump_node()?;
        self.skip_ws();
        if self.eat_char(b'(') {
            let inner = self.parse_expr(depth + 1)?;
            if !self.eat_char(b')') {
                return Err(PromError::bad_data("missing closing ')'"));
            }
            return Ok(inner);
        }
        if self
            .peek_byte()
            .is_some_and(|b| b.is_ascii_digit() || b == b'.')
        {
            return self.parse_number();
        }
        let ident = self.parse_ident()?;
        self.skip_ws();

        if let Some(agg_op) = match_agg_op(&ident) {
            return self.parse_aggregation(agg_op, depth);
        }
        if ident.eq_ignore_ascii_case("topk") {
            return self.parse_topk(depth);
        }
        if ident == "histogram_quantile" {
            return self.parse_histogram_quantile(depth);
        }
        if ident == "clamp_min" {
            return self.parse_clamp_min(depth);
        }
        if let Some(rf) = match_range_func(&ident) {
            if !self.eat_char(b'(') {
                return Err(PromError::bad_data(format!("expected '(' after {ident}")));
            }
            self.skip_ws();
            let m_name = self.parse_ident()?;
            let mut sel = self.parse_selector_tail(m_name)?;
            if sel.range_secs.is_none() {
                return Err(PromError::bad_data(format!(
                    "{ident}() requires a range vector [duration]"
                )));
            }
            if !self.eat_char(b')') {
                return Err(PromError::bad_data(format!(
                    "missing closing ')' in {ident}()"
                )));
            }
            // Also accept trailing offset just before ')' (handled in selector_tail).
            let _ = &mut sel;
            return Ok(Expr::RangeCall {
                func: rf,
                selector: sel,
            });
        }

        let sel = self.parse_selector_tail(ident)?;
        if sel.range_secs.is_some() {
            return Err(PromError::bad_data(
                "bare range vector requires a range function such as increase() or rate()",
            ));
        }
        Ok(Expr::Selector(sel))
    }

    fn parse_aggregation(&mut self, op: AggOp, depth: usize) -> Result<Expr, PromError> {
        let mut grouping = self.parse_optional_grouping()?;
        if !self.eat_char(b'(') {
            return Err(PromError::bad_data("expected '(' in aggregation"));
        }
        let inner = self.parse_expr(depth + 1)?;
        if !self.eat_char(b')') {
            return Err(PromError::bad_data("missing ')' in aggregation"));
        }
        if matches!(grouping, Grouping::All) {
            grouping = self.parse_optional_grouping()?;
        }
        Ok(Expr::Aggregate {
            op,
            grouping,
            expr: Box::new(inner),
        })
    }

    fn parse_optional_grouping(&mut self) -> Result<Grouping, PromError> {
        self.skip_ws();
        let saved = self.pos;
        let Ok(kw) = self.parse_ident() else {
            self.pos = saved;
            return Ok(Grouping::All);
        };
        let is_by = kw.eq_ignore_ascii_case("by");
        let is_without = kw.eq_ignore_ascii_case("without");
        if !is_by && !is_without {
            self.pos = saved;
            return Ok(Grouping::All);
        }
        if !self.eat_char(b'(') {
            return Err(PromError::bad_data(format!("expected '(' after {kw}")));
        }
        let mut labels = Vec::new();
        self.skip_ws();
        if !self.eat_char(b')') {
            loop {
                labels.push(self.parse_ident()?);
                if self.eat_char(b',') {
                    continue;
                }
                if self.eat_char(b')') {
                    break;
                }
                return Err(PromError::bad_data("expected ',' or ')' in label list"));
            }
        }
        Ok(if is_by {
            Grouping::By(labels)
        } else {
            Grouping::Without(labels)
        })
    }

    fn parse_topk(&mut self, depth: usize) -> Result<Expr, PromError> {
        if !self.eat_char(b'(') {
            return Err(PromError::bad_data("expected '(' after topk"));
        }
        let Expr::Number(k_f) = self.parse_number()? else {
            return Err(PromError::bad_data("topk requires integer k"));
        };
        let k = (k_f.round() as isize).clamp(1, 100) as usize;
        if !self.eat_char(b',') {
            return Err(PromError::bad_data("expected ',' after topk k"));
        }
        let inner = self.parse_expr(depth + 1)?;
        if !self.eat_char(b')') {
            return Err(PromError::bad_data("missing ')' in topk"));
        }
        Ok(Expr::TopK {
            k,
            expr: Box::new(inner),
        })
    }

    fn parse_histogram_quantile(&mut self, depth: usize) -> Result<Expr, PromError> {
        if !self.eat_char(b'(') {
            return Err(PromError::bad_data("expected '(' after histogram_quantile"));
        }
        let Expr::Number(phi) = self.parse_number()? else {
            return Err(PromError::bad_data(
                "histogram_quantile requires numeric quantile",
            ));
        };
        if !(0.0..=1.0).contains(&phi) {
            return Err(PromError::bad_data(
                "histogram_quantile phi must be in 0..=1",
            ));
        }
        if !self.eat_char(b',') {
            return Err(PromError::bad_data(
                "expected ',' in histogram_quantile(phi, expr)",
            ));
        }
        let inner = self.parse_expr(depth + 1)?;
        if !self.eat_char(b')') {
            return Err(PromError::bad_data("missing ')' in histogram_quantile"));
        }
        Ok(Expr::HistogramQuantile {
            phi,
            expr: Box::new(inner),
        })
    }

    fn parse_clamp_min(&mut self, depth: usize) -> Result<Expr, PromError> {
        if !self.eat_char(b'(') {
            return Err(PromError::bad_data("expected '(' after clamp_min"));
        }
        let inner = self.parse_expr(depth + 1)?;
        if !self.eat_char(b',') {
            return Err(PromError::bad_data("expected ',' in clamp_min(expr, min)"));
        }
        let Expr::Number(min) = self.parse_number()? else {
            return Err(PromError::bad_data("clamp_min requires numeric minimum"));
        };
        if !self.eat_char(b')') {
            return Err(PromError::bad_data("missing ')' in clamp_min"));
        }
        Ok(Expr::ClampMin {
            expr: Box::new(inner),
            min,
        })
    }

    fn parse_selector_tail(&mut self, metric: String) -> Result<VectorSelector, PromError> {
        let mut matchers = Vec::new();
        if self.eat_char(b'{') {
            self.skip_ws();
            if !self.eat_char(b'}') {
                loop {
                    self.skip_ws();
                    if self.eat_char(b'}') {
                        break;
                    }
                    if matchers.len() >= MAX_MATCHERS_PER_SELECTOR {
                        return Err(PromError::bad_data(format!(
                            "selector exceeds maximum {MAX_MATCHERS_PER_SELECTOR} label matchers"
                        )));
                    }
                    let label_name = self.parse_ident()?;
                    self.skip_ws();
                    let op_str = self.parse_match_op()?;
                    let val = self.parse_quoted_string()?;
                    let op = match op_str {
                        "=" => LabelMatchOp::Eq(val),
                        "!=" => LabelMatchOp::NotEq(val),
                        "=~" => LabelMatchOp::RegexEq(compile_anchored_regex(&val)?),
                        "!~" => LabelMatchOp::RegexNotEq(compile_anchored_regex(&val)?),
                        _ => unreachable!(),
                    };
                    matchers.push(LabelMatcher {
                        name: label_name,
                        op,
                    });
                    if self.eat_char(b',') {
                        continue;
                    }
                    if self.eat_char(b'}') {
                        break;
                    }
                    return Err(PromError::bad_data("expected ',' or '}' in label selector"));
                }
            }
        }

        let mut range_secs = None;
        if self.eat_char(b'[') {
            let start = self.pos;
            while let Some(b) = self.peek_byte() {
                if b == b']' {
                    break;
                }
                self.pos += 1;
            }
            let raw_dur = &self.s[start..self.pos];
            if !self.eat_char(b']') {
                return Err(PromError::bad_data("missing closing ']' on range selector"));
            }
            range_secs = Some(parse_prom_duration_secs(raw_dur)?);
        }

        let mut offset_secs = 0i64;
        self.skip_ws();
        let saved = self.pos;
        if let Ok(kw) = self.parse_ident() {
            if kw == "offset" {
                self.skip_ws();
                let dur_start = self.pos;
                while let Some(b) = self.peek_byte() {
                    if b.is_ascii_alphanumeric() {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                offset_secs = parse_prom_duration_secs(&self.s[dur_start..self.pos])?;
            } else {
                self.pos = saved;
            }
        } else {
            self.pos = saved;
        }

        Ok(VectorSelector {
            metric,
            matchers,
            range_secs,
            offset_secs,
        })
    }

    fn parse_match_op(&mut self) -> Result<&'static str, PromError> {
        let rem = &self.s[self.pos..];
        for op in ["=~", "!~", "!=", "="] {
            if rem.starts_with(op) {
                self.pos += op.len();
                return Ok(op);
            }
        }
        Err(PromError::bad_data(
            "expected label operator (=, !=, =~, !~)",
        ))
    }

    fn parse_quoted_string(&mut self) -> Result<String, PromError> {
        self.skip_ws();
        let quote = match self.peek_byte() {
            Some(q @ (b'"' | b'\'')) => {
                self.pos += 1;
                q
            }
            _ => return Err(PromError::bad_data("expected quoted label value")),
        };
        let mut out = String::new();
        while self.pos < self.s.len() {
            let ch = self.s[self.pos..]
                .chars()
                .next()
                .ok_or_else(|| PromError::bad_data("unterminated string"))?;
            self.pos += ch.len_utf8();
            if ch as u32 == u32::from(quote) {
                return Ok(out);
            }
            if ch == '\\' {
                let esc = self.s[self.pos..]
                    .chars()
                    .next()
                    .ok_or_else(|| PromError::bad_data("dangling escape in string"))?;
                self.pos += esc.len_utf8();
                match esc {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    '\'' => out.push('\''),
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
            } else {
                out.push(ch);
            }
        }
        Err(PromError::bad_data("unterminated string literal"))
    }

    fn parse_ident(&mut self) -> Result<String, PromError> {
        self.skip_ws();
        let start = self.pos;
        while let Some(b) = self.peek_byte() {
            if b.is_ascii_alphanumeric() || b == b'_' || b == b':' {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start || self.s.as_bytes()[start].is_ascii_digit() {
            return Err(PromError::bad_data("expected metric or label identifier"));
        }
        Ok(self.s[start..self.pos].to_string())
    }

    fn parse_number(&mut self) -> Result<Expr, PromError> {
        self.skip_ws();
        let start = self.pos;
        while let Some(b) = self.peek_byte() {
            if b.is_ascii_digit() || b == b'.' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let val = self.s[start..self.pos]
            .parse::<f64>()
            .map_err(|_| PromError::bad_data("invalid numeric literal"))?;
        if !val.is_finite() {
            return Err(PromError::bad_data("numeric literal out of range"));
        }
        Ok(Expr::Number(val))
    }
}

fn match_agg_op(s: &str) -> Option<AggOp> {
    match s.to_ascii_lowercase().as_str() {
        "sum" => Some(AggOp::Sum),
        "avg" => Some(AggOp::Avg),
        "min" => Some(AggOp::Min),
        "max" => Some(AggOp::Max),
        "count" => Some(AggOp::Count),
        _ => None,
    }
}

fn match_range_func(s: &str) -> Option<RangeFunc> {
    match s {
        "increase" => Some(RangeFunc::Increase),
        "rate" => Some(RangeFunc::Rate),
        "sum_over_time" => Some(RangeFunc::SumOverTime),
        "avg_over_time" => Some(RangeFunc::AvgOverTime),
        "max_over_time" => Some(RangeFunc::MaxOverTime),
        _ => None,
    }
}

fn compile_anchored_regex(pat: &str) -> Result<regex::Regex, PromError> {
    let anchored = format!("^(?:{pat})$");
    RegexBuilder::new(&anchored)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| PromError::bad_data(format!("invalid label regex: {e}")))
}

// ============================================================================
// Pure Evaluation Engine over Bounded Series Data
// ============================================================================

#[derive(Debug, Clone)]
pub struct EvalSample {
    pub labels: Labels,
    pub value: f64,
}

#[derive(Debug, Clone)]
pub enum EvalValue {
    Scalar(f64),
    Vector(Vec<EvalSample>),
}

/// Pure synchronous callback interface populated from bounded O(1) SQLite
/// rollups + live in-memory metrics prior to evaluating an AST across 1 or `K`
/// timestamps.
pub trait PromDataSource {
    fn now_ts(&self) -> i64;
    fn select_instant(&self, selector: &VectorSelector, eval_ts: i64) -> Vec<EvalSample>;
    fn select_range(
        &self,
        func: RangeFunc,
        selector: &VectorSelector,
        eval_ts: i64,
    ) -> Vec<EvalSample>;
}

pub fn eval_instant(
    expr: &Expr,
    ds: &dyn PromDataSource,
    eval_ts: i64,
) -> Result<EvalValue, PromError> {
    match expr {
        Expr::Number(n) => Ok(EvalValue::Scalar(*n)),
        Expr::Selector(sel) => {
            let shifted = eval_ts.saturating_sub(sel.offset_secs);
            Ok(EvalValue::Vector(ds.select_instant(sel, shifted)))
        }
        Expr::RangeCall { func, selector } => {
            let shifted = eval_ts.saturating_sub(selector.offset_secs);
            Ok(EvalValue::Vector(ds.select_range(*func, selector, shifted)))
        }
        Expr::Aggregate { op, grouping, expr } => {
            let inner = into_vector(eval_instant(expr, ds, eval_ts)?);
            Ok(EvalValue::Vector(apply_aggregation(*op, grouping, inner)))
        }
        Expr::TopK { k, expr } => {
            let mut inner = into_vector(eval_instant(expr, ds, eval_ts)?);
            inner.sort_by(|a, b| {
                b.value
                    .partial_cmp(&a.value)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            inner.truncate(*k);
            Ok(EvalValue::Vector(inner))
        }
        Expr::HistogramQuantile { phi, expr } => {
            let inner = into_vector(eval_instant(expr, ds, eval_ts)?);
            Ok(EvalValue::Vector(eval_histogram_quantile(*phi, inner)))
        }
        Expr::ClampMin { expr, min } => match eval_instant(expr, ds, eval_ts)? {
            EvalValue::Scalar(v) => Ok(EvalValue::Scalar(v.max(*min))),
            EvalValue::Vector(mut vec) => {
                for s in &mut vec {
                    if !s.value.is_nan() {
                        s.value = s.value.max(*min);
                    }
                }
                Ok(EvalValue::Vector(vec))
            }
        },
        Expr::Binary { op, lhs, rhs } => {
            let l = eval_instant(lhs, ds, eval_ts)?;
            let r = eval_instant(rhs, ds, eval_ts)?;
            Ok(eval_binary(*op, l, r))
        }
    }
}

fn into_vector(val: EvalValue) -> Vec<EvalSample> {
    match val {
        EvalValue::Vector(v) => v,
        EvalValue::Scalar(n) => vec![EvalSample {
            labels: Labels::new(),
            value: n,
        }],
    }
}

fn apply_bin_op(op: BinOp, a: f64, b: f64) -> f64 {
    match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        BinOp::Div => {
            if b == 0.0 {
                f64::NAN
            } else {
                a / b
            }
        }
    }
}

fn eval_binary(op: BinOp, lhs: EvalValue, rhs: EvalValue) -> EvalValue {
    match (lhs, rhs) {
        (EvalValue::Scalar(a), EvalValue::Scalar(b)) => EvalValue::Scalar(apply_bin_op(op, a, b)),
        (EvalValue::Vector(mut vec), EvalValue::Scalar(b)) => {
            for s in &mut vec {
                s.labels.remove("__name__");
                s.value = apply_bin_op(op, s.value, b);
            }
            EvalValue::Vector(vec)
        }
        (EvalValue::Scalar(a), EvalValue::Vector(mut vec)) => {
            for s in &mut vec {
                s.labels.remove("__name__");
                s.value = apply_bin_op(op, a, s.value);
            }
            EvalValue::Vector(vec)
        }
        (EvalValue::Vector(lvec), EvalValue::Vector(rvec)) => {
            let mut rmap: BTreeMap<Labels, f64> = BTreeMap::new();
            for mut r in rvec {
                r.labels.remove("__name__");
                rmap.insert(r.labels, r.value);
            }
            let mut out = Vec::new();
            for mut l in lvec {
                l.labels.remove("__name__");
                if let Some(&rv) = rmap.get(&l.labels) {
                    out.push(EvalSample {
                        labels: l.labels,
                        value: apply_bin_op(op, l.value, rv),
                    });
                }
            }
            EvalValue::Vector(out)
        }
    }
}

fn apply_aggregation(op: AggOp, grouping: &Grouping, samples: Vec<EvalSample>) -> Vec<EvalSample> {
    let mut groups: BTreeMap<Labels, Vec<f64>> = BTreeMap::new();
    for s in samples {
        if s.value.is_nan() {
            continue;
        }
        let mut key = Labels::new();
        match grouping {
            Grouping::All => {}
            Grouping::By(keep) => {
                for k in keep {
                    if let Some(v) = s.labels.get(k) {
                        key.insert(k.clone(), v.clone());
                    }
                }
            }
            Grouping::Without(drop_keys) => {
                for (k, v) in &s.labels {
                    if k != "__name__" && !drop_keys.iter().any(|d| d == k) {
                        key.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        groups.entry(key).or_default().push(s.value);
    }

    groups
        .into_iter()
        .filter_map(|(labels, vals)| {
            if vals.is_empty() {
                return None;
            }
            let v = match op {
                AggOp::Sum => vals.iter().sum(),
                AggOp::Avg => vals.iter().sum::<f64>() / vals.len() as f64,
                AggOp::Min => vals.iter().copied().fold(f64::INFINITY, f64::min),
                AggOp::Max => vals.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                AggOp::Count => vals.len() as f64,
            };
            Some(EvalSample { labels, value: v })
        })
        .collect()
}

fn eval_histogram_quantile(phi: f64, samples: Vec<EvalSample>) -> Vec<EvalSample> {
    let mut by_series: BTreeMap<Labels, Vec<(f64, f64)>> = BTreeMap::new();
    for mut s in samples {
        let Some(le_str) = s.labels.remove("le") else {
            continue;
        };
        s.labels.remove("__name__");
        let bound = if le_str == "+Inf" || le_str.eq_ignore_ascii_case("inf") {
            f64::INFINITY
        } else if let Ok(b) = le_str.parse::<f64>() {
            b
        } else {
            continue;
        };
        by_series
            .entry(s.labels)
            .or_default()
            .push((bound, s.value));
    }

    let mut out = Vec::new();
    for (labels, mut buckets) in by_series {
        buckets.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let q = compute_bucket_quantile(phi, &buckets);
        out.push(EvalSample { labels, value: q });
    }
    out
}

fn compute_bucket_quantile(phi: f64, buckets: &[(f64, f64)]) -> f64 {
    if buckets.len() < 2 {
        return f64::NAN;
    }
    let Some(&(last_bound, total)) = buckets.last() else {
        return f64::NAN;
    };
    if !last_bound.is_infinite() || total <= 0.0 {
        return f64::NAN;
    }
    let rank = phi * total;
    for (idx, &(upper, cum_count)) in buckets.iter().enumerate() {
        if cum_count >= rank {
            if upper.is_infinite() {
                return buckets
                    .get(idx.saturating_sub(1))
                    .map_or(0.0, |(prev_u, _)| *prev_u);
            }
            let (lower, prev_count) = if idx == 0 {
                (0.0, 0.0)
            } else {
                buckets[idx - 1]
            };
            let bucket_count = cum_count - prev_count;
            if bucket_count <= 0.0 {
                return upper;
            }
            return lower + (upper - lower) * ((rank - prev_count) / bucket_count);
        }
    }
    buckets
        .get(buckets.len().saturating_sub(2))
        .map_or(f64::NAN, |(u, _)| *u)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockSource {
        now: i64,
        instant: Vec<(String, Labels, f64)>,
        buckets: Vec<(String, Labels, i64, f64)>,
    }

    impl PromDataSource for MockSource {
        fn now_ts(&self) -> i64 {
            self.now
        }

        fn select_instant(&self, selector: &VectorSelector, _eval_ts: i64) -> Vec<EvalSample> {
            self.instant
                .iter()
                .filter(|(m, l, _)| {
                    *m == selector.metric && selector.matchers.iter().all(|mc| mc.matches(l))
                })
                .map(|(_, l, v)| EvalSample {
                    labels: l.clone(),
                    value: *v,
                })
                .collect()
        }

        fn select_range(
            &self,
            func: RangeFunc,
            selector: &VectorSelector,
            eval_ts: i64,
        ) -> Vec<EvalSample> {
            let win = selector.range_secs.unwrap_or(300).max(1);
            let start = eval_ts - win;
            let mut by_labels: BTreeMap<Labels, Vec<f64>> = BTreeMap::new();
            for (m, l, ts, val) in &self.buckets {
                if *m == selector.metric
                    && *ts > start
                    && *ts <= eval_ts
                    && selector.matchers.iter().all(|mc| mc.matches(l))
                {
                    by_labels.entry(l.clone()).or_default().push(*val);
                }
            }
            by_labels
                .into_iter()
                .map(|(labels, vals)| {
                    let sum: f64 = vals.iter().sum();
                    let value = match func {
                        RangeFunc::Increase | RangeFunc::SumOverTime => sum,
                        RangeFunc::Rate => sum / win as f64,
                        RangeFunc::AvgOverTime => sum / vals.len() as f64,
                        RangeFunc::MaxOverTime => {
                            vals.into_iter().fold(f64::NEG_INFINITY, f64::max)
                        }
                    };
                    EvalSample { labels, value }
                })
                .collect()
        }
    }

    fn lbl(pairs: &[(&str, &str)]) -> Labels {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn test_selectors_regex_aggregations_and_topk() {
        let ds = MockSource {
            now: 10_000,
            instant: vec![
                (
                    "sashiko_bugs_by_subsystem".into(),
                    lbl(&[("subsystem", "net"), ("facet", "open")]),
                    12.0,
                ),
                (
                    "sashiko_bugs_by_subsystem".into(),
                    lbl(&[("subsystem", "mm"), ("facet", "open")]),
                    30.0,
                ),
                (
                    "sashiko_bugs_by_subsystem".into(),
                    lbl(&[("subsystem", "fs"), ("facet", "open")]),
                    5.0,
                ),
                (
                    "sashiko_bugs_by_subsystem".into(),
                    lbl(&[("subsystem", "net"), ("facet", "fixed")]),
                    8.0,
                ),
            ],
            buckets: Vec::new(),
        };

        let q = parse_promql(
            r#"topk(2, sashiko_bugs_by_subsystem{facet="open", subsystem=~"net|mm|fs"})"#,
        )
        .unwrap();
        let EvalValue::Vector(top) = eval_instant(&q, &ds, 10_000).unwrap() else {
            panic!("expected vector");
        };
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].labels["subsystem"], "mm");
        assert_eq!(top[0].value, 30.0);
        assert_eq!(top[1].labels["subsystem"], "net");
        assert_eq!(top[1].value, 12.0);

        let sum_by = parse_promql(r#"sum by (facet) (sashiko_bugs_by_subsystem)"#).unwrap();
        let EvalValue::Vector(grouped) = eval_instant(&sum_by, &ds, 10_000).unwrap() else {
            panic!("expected vector");
        };
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].labels["facet"], "fixed");
        assert_eq!(grouped[0].value, 8.0);
        assert_eq!(grouped[1].labels["facet"], "open");
        assert_eq!(grouped[1].value, 47.0);
    }

    #[test]
    fn test_range_offset_binary_clamp_and_histogram_quantile() {
        let ds = MockSource {
            now: 20_000,
            instant: Vec::new(),
            buckets: vec![
                ("m_total".into(), lbl(&[("source", "nntp")]), 19_800, 60.0),
                ("m_total".into(), lbl(&[("source", "nntp")]), 16_200, 30.0),
                ("lat_bucket".into(), lbl(&[("le", "1")]), 19_900, 20.0),
                ("lat_bucket".into(), lbl(&[("le", "5")]), 19_900, 80.0),
                ("lat_bucket".into(), lbl(&[("le", "+Inf")]), 19_900, 100.0),
            ],
        };

        let ratio = parse_promql(
            r#"sum(increase(m_total[1h])) / clamp_min(sum(increase(m_total[1h] offset 1h)), 1)"#,
        )
        .unwrap();
        let EvalValue::Vector(v) = eval_instant(&ratio, &ds, 20_000).unwrap() else {
            panic!("expected vector");
        };
        assert_eq!(v.len(), 1);
        assert!((v[0].value - 2.0).abs() < 1e-6);

        let p90 =
            parse_promql(r#"histogram_quantile(0.9, sum by (le) (increase(lat_bucket[1h])))"#)
                .unwrap();
        let EvalValue::Vector(qv) = eval_instant(&p90, &ds, 20_000).unwrap() else {
            panic!("expected vector");
        };
        assert_eq!(qv.len(), 1);
        // rank=90 falls above le=5 (80) into +Inf, clamping cleanly to upper finite bucket 5.0.
        assert!((qv[0].value - 5.0).abs() < 1e-6);

        let p50 =
            parse_promql(r#"histogram_quantile(0.5, sum by (le) (increase(lat_bucket[1h])))"#)
                .unwrap();
        let EvalValue::Vector(q50) = eval_instant(&p50, &ds, 20_000).unwrap() else {
            panic!("expected vector");
        };
        // rank=50 in bucket (1..5] with prev=20, count=60 -> 1 + 4*(30/60) = 3.0
        assert!((q50[0].value - 3.0).abs() < 1e-6);
    }

    #[test]
    fn test_dos_bounds_reject_oversized_and_deep_queries() {
        let huge = "a".repeat(MAX_QUERY_LEN + 1);
        assert!(parse_promql(&huge).is_err());
        assert!(parse_promql("((((((((((1))))))))))").is_err());
        assert!(parse_promql("m_total[5m]").is_err());
        assert!(parse_prom_duration_secs("1e20").is_err());
        assert!(parse_prom_duration_secs("99999y").is_err());
        assert!(parse_prom_timestamp("1e20", 1_700_000_000).is_err());
        assert!(parse_prom_timestamp("1960-01-01T00:00:00Z", 1_700_000_000).is_err());
        // AST node explosion (e.g. a+a+... > MAX_AST_NODES) must be rejected.
        let wide_sum = vec!["1"; MAX_AST_NODES].join("+");
        assert!(parse_promql(&wide_sum).is_err());
        // Excessive matchers in a single selector must be rejected.
        let many_matchers = (0..=MAX_MATCHERS_PER_SELECTOR)
            .map(|i| format!(r#"k{i}="v""#))
            .collect::<Vec<_>>()
            .join(",");
        assert!(parse_promql(&format!("m{{{many_matchers}}}")).is_err());
    }
}
