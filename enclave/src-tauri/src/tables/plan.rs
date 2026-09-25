//! A question → a calculation over one table (ADR-0022).
//!
//! 1. `load_candidates`: the tables of the spreadsheets retrieval just
//!    found for this question (under RLS), at most `MAX_CANDIDATES`.
//! 2. `planner_messages` + `plan_schema`: the model sees each table's
//!    columns and sample values and answers with JSON constrained by a
//!    grammar built from those very columns — it can name no column, table
//!    or operation that does not exist.
//! 3. `parse_plan`: the answer is checked against the catalog (types fit
//!    the operations, values parse) — anything off, and the question falls
//!    back to ordinary retrieval.
//! 4. `execute`: the plan compiles into one aggregate over `sheet_rows`.
//!    Column positions and values are bind parameters; the SQL text is
//!    assembled only from fixed fragments. Runs on app_pool in the caller's
//!    transaction, so RLS decides which rows exist (ADR-0008).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::{parse_date, parse_number, Column, ColumnType};

/// Tables shown to the planner. Each costs prompt tokens; the ones that
/// matter are those whose rows retrieval ranked first.
const MAX_CANDIDATES: usize = 3;

/// Filters in one plan — more than a question states in practice, and a
/// bound on what the grammar lets the model produce.
const MAX_FILTERS: usize = 4;

/// Values of a text column listed in the planner's prompt.
const PROMPT_VALUES: usize = 12;

/// Groups listed in the result; the rest are counted, not shown.
const MAX_GROUPS: i64 = 10;

/// A table the question may be about, as the planner sees it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub table_id:    Uuid,
    pub document_id: Uuid,
    pub filename:    String,
    pub sheet:       String,
    pub row_count:   i64,
    pub columns:     Vec<Column>,
}

impl Candidate {
    /// The name the planner uses for it: T1, T2, …
    fn label(index: usize) -> String {
        format!("T{}", index + 1)
    }
}

/// The spreadsheet tables among `document_ids` (retrieval's documents, best
/// first), in that order. Runs in the caller's transaction: RLS applies.
pub async fn load_candidates(conn: &mut PgConnection, document_ids: &[Uuid]) -> Result<Vec<Candidate>> {
    let mut ordered: Vec<Uuid> = Vec::new();
    for id in document_ids {
        if !ordered.contains(id) {
            ordered.push(*id);
        }
    }
    let rows = sqlx::query(
        r#"
        SELECT t.id, t.document_id, d.title, t.sheet, t.row_count, t.columns
        FROM sheet_tables t
        JOIN documents d ON d.id = t.document_id
        JOIN unnest($1::uuid[]) WITH ORDINALITY AS o(id, rank) ON o.id = t.document_id
        WHERE d.status = 'ready' AND d.deleted_at IS NULL
        ORDER BY o.rank, t.sheet_index
        LIMIT $2
        "#,
    )
    .bind(&ordered)
    .bind(MAX_CANDIDATES as i64)
    .fetch_all(&mut *conn)
    .await?;

    rows.iter().map(candidate_from_row).collect()
}

/// One table by id, as this user sees it (RLS applies) — for a plan the
/// user chose in a clarification. None if the table is not visible or its
/// document is gone.
pub async fn load_candidate(conn: &mut PgConnection, table_id: Uuid) -> Result<Option<Candidate>> {
    let row = sqlx::query(
        r#"
        SELECT t.id, t.document_id, d.title, t.sheet, t.row_count, t.columns
        FROM sheet_tables t
        JOIN documents d ON d.id = t.document_id
        WHERE t.id = $1 AND d.status = 'ready' AND d.deleted_at IS NULL
        "#,
    )
    .bind(table_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref().map(candidate_from_row).transpose()
}

fn candidate_from_row(r: &sqlx::postgres::PgRow) -> Result<Candidate> {
    Ok(Candidate {
        table_id:    r.try_get("id")?,
        document_id: r.try_get("document_id")?,
        filename:    r.try_get("title")?,
        sheet:       r.try_get("sheet")?,
        row_count:   r.try_get::<i32, _>("row_count")? as i64,
        columns:     serde_json::from_value(r.try_get("columns")?)?,
    })
}

// ─── What the planner sees ───────────────────────────────────────────────────

const PLANNER_SYSTEM: &str = r#"You decide whether a question can be answered by a calculation over one of the spreadsheet tables in the user's message, and if so, you write the calculation plan as JSON.

Plan a calculation only when the question asks for a count, a sum, an average, a minimum or a maximum over rows ("how many", "total", "average", "largest", "per manager", "by year"), possibly for rows matching conditions. Otherwise — a question about one specific record (for example a contract by its number), a question about text, or a question none of the tables can answer — reply {"answerable": false}.

Plan fields, in this order:
- table: the table's label (T1, T2, …).
- metric: what to compute. "count" counts rows (column null) — for "how many". "sum", "avg", "min", "max" of a number column; "min" / "max" of a date column for "earliest" / "latest".
- group_by: null for one total. A column for "per …", "by …", "each …", "for every …" — one result per value of that column. Also when the question asks for a number together with a column without naming one value of it ("number of contracts and the manager's name", "who has the most") — the answer needs one number per value of that column. For a date column also the period: year, month or day.
- filters: only the conditions the question states — none if it states none. Text columns: "contains" / "not_contains"; the value is spelled exactly as in the column's values. Number columns: = != > >= < <= and a plain number. Date columns: = != > >= < <= and YYYY, YYYY-MM or YYYY-MM-DD; "= 2021" means during 2021.
- order: "desc" (largest first) unless the question asks for the smallest.

Examples, for a table T1 with columns "ФИО" (text), "Отдел" (text): values "Продажи", "ИТ", "Склад"; "Оклад" (number); "Дата приёма" (date):
- "Сколько сотрудников в ИТ?" → {"answerable": true, "table": "T1", "metric": {"fn": "count", "column": null}, "group_by": null, "filters": [{"column": "Отдел", "op": "contains", "value": "ИТ"}], "order": "desc"}
- "Количество сотрудников и отдел" → {"answerable": true, "table": "T1", "metric": {"fn": "count", "column": null}, "group_by": {"column": "Отдел"}, "filters": [], "order": "desc"}
- "Какой средний оклад по отделам?" → {"answerable": true, "table": "T1", "metric": {"fn": "avg", "column": "Оклад"}, "group_by": {"column": "Отдел"}, "filters": [], "order": "desc"}
- "Сколько человек приняли в 2022 году?" → {"answerable": true, "table": "T1", "metric": {"fn": "count", "column": null}, "group_by": null, "filters": [{"column": "Дата приёма", "op": "=", "value": "2022"}], "order": "desc"}
- "Сколько приёмов по годам?" → {"answerable": true, "table": "T1", "metric": {"fn": "count", "column": null}, "group_by": {"column": "Дата приёма", "period": "year"}, "filters": [], "order": "desc"}
- "Сколько человек приняли с 2021 по 2022 год?" → {"answerable": true, "table": "T1", "metric": {"fn": "count", "column": null}, "group_by": null, "filters": [{"column": "Дата приёма", "op": ">=", "value": "2021"}, {"column": "Дата приёма", "op": "<=", "value": "2022"}], "order": "desc"}
- "В каком отделе самая большая сумма окладов?" → {"answerable": true, "table": "T1", "metric": {"fn": "sum", "column": "Оклад"}, "group_by": {"column": "Отдел"}, "filters": [], "order": "desc"}
- "Какой самый большой оклад?" → {"answerable": true, "table": "T1", "metric": {"fn": "max", "column": "Оклад"}, "group_by": null, "filters": [], "order": "desc"}
- "Какой оклад у Смирнова?" → {"answerable": false}"#;

/// System and user messages for the planner call.
pub fn planner_messages(candidates: &[Candidate], question: &str) -> (&'static str, String) {
    let mut tables = String::new();
    for (i, c) in candidates.iter().enumerate() {
        tables.push_str(&format!(
            "Table {}: file «{}», sheet «{}», {} rows. Columns:\n",
            Candidate::label(i),
            c.filename,
            c.sheet,
            c.row_count
        ));
        for col in &c.columns {
            let detail = match col.kind {
                ColumnType::Text if col.distinct as usize > col.top.len() && col.top.iter().all(|(_, n)| *n == 1) => {
                    let examples: Vec<String> = col.top.iter().take(3).map(|(v, _)| format!("\"{v}\"")).collect();
                    format!("unique values, e.g. {}", examples.join(", "))
                }
                ColumnType::Text => {
                    let shown = col.top.iter().take(PROMPT_VALUES);
                    let values: Vec<String> = shown.map(|(v, n)| format!("\"{v}\" ({n} rows)")).collect();
                    let more = col.distinct - values.len() as i64;
                    let tail = if more > 0 { format!(" and {more} more") } else { String::new() };
                    format!("values: {}{tail}", values.join(", "))
                }
                ColumnType::Number | ColumnType::Date => format!(
                    "from {} to {}",
                    col.min.as_deref().unwrap_or("?"),
                    col.max.as_deref().unwrap_or("?")
                ),
            };
            let kind = match col.kind {
                ColumnType::Number => "number",
                ColumnType::Date => "date",
                ColumnType::Text => "text",
            };
            tables.push_str(&format!("- \"{}\" ({kind}): {detail}\n", col.name));
        }
        tables.push('\n');
    }
    (PLANNER_SYSTEM, format!("{tables}Question: {question}"))
}

fn names_of(columns: &[Column], kinds: &[ColumnType]) -> Vec<String> {
    columns.iter().filter(|c| kinds.contains(&c.kind)).map(|c| c.name.clone()).collect()
}

/// A JSON schema the model's answer must match (llama-server turns it into
/// a grammar): one variant per candidate table, listing that table's own
/// columns, split by type so that a filter or metric can only pair a column
/// with operations its type supports.
pub fn plan_schema(candidates: &[Candidate]) -> Value {
    let mut variants = vec![json!({
        "type": "object",
        "properties": { "answerable": { "const": false } },
        "required": ["answerable"],
        "additionalProperties": false
    })];

    for (i, c) in candidates.iter().enumerate() {
        let number = names_of(&c.columns, &[ColumnType::Number]);
        let date = names_of(&c.columns, &[ColumnType::Date]);
        let comparisons = json!(["=", "!=", ">", ">=", "<", "<="]);

        let filter = |columns: &[String], ops: Value| {
            json!({
                "type": "object",
                "properties": {
                    "column": { "enum": columns },
                    "op": { "enum": ops },
                    "value": { "type": "string" }
                },
                "required": ["column", "op", "value"],
                "additionalProperties": false
            })
        };
        let mut filters = Vec::new();
        // A closed text column ("Статус", "Отдел") gets a variant of its own
        // whose value can only be one of its actual values: the model cannot
        // misspell "Петрова А.С." as "Петров А.С." and match nothing.
        let mut open_text = Vec::new();
        for col in c.columns.iter().filter(|c| c.kind == ColumnType::Text) {
            match col.closed_values() {
                Some(values) => filters.push(json!({
                    "type": "object",
                    "properties": {
                        "column": { "const": col.name },
                        "op": { "enum": ["contains", "not_contains"] },
                        "value": { "enum": values }
                    },
                    "required": ["column", "op", "value"],
                    "additionalProperties": false
                })),
                None => open_text.push(col.name.clone()),
            }
        }
        if !open_text.is_empty() {
            filters.push(filter(&open_text, json!(["contains", "not_contains"])));
        }
        if !number.is_empty() {
            filters.push(filter(&number, comparisons.clone()));
        }
        if !date.is_empty() {
            filters.push(filter(&date, comparisons.clone()));
        }

        let mut groups = vec![json!({ "type": "null" })];
        let plain: Vec<String> = names_of(&c.columns, &[ColumnType::Text, ColumnType::Number]);
        if !plain.is_empty() {
            groups.push(json!({
                "type": "object",
                "properties": { "column": { "enum": plain } },
                "required": ["column"],
                "additionalProperties": false
            }));
        }
        if !date.is_empty() {
            groups.push(json!({
                "type": "object",
                "properties": {
                    "column": { "enum": date },
                    "period": { "enum": ["year", "month", "day"] }
                },
                "required": ["column", "period"],
                "additionalProperties": false
            }));
        }

        let metric = |functions: Value, column: Value| {
            json!({
                "type": "object",
                "properties": { "fn": { "enum": functions }, "column": column },
                "required": ["fn", "column"],
                "additionalProperties": false
            })
        };
        let mut metrics = vec![metric(json!(["count"]), json!({ "type": "null" }))];
        if !number.is_empty() {
            metrics.push(metric(json!(["sum", "avg", "min", "max"]), json!({ "enum": number })));
        }
        if !date.is_empty() {
            metrics.push(metric(json!(["min", "max"]), json!({ "enum": date })));
        }

        let filter_schema = if filters.is_empty() {
            json!({ "type": "array", "maxItems": 0 })
        } else {
            json!({ "type": "array", "items": { "anyOf": filters }, "maxItems": MAX_FILTERS })
        };

        variants.push(json!({
            "type": "object",
            "properties": {
                // Written in this order (serde_json keeps it): what to
                // compute is decided before the conditions.
                "answerable": { "const": true },
                "table": { "const": Candidate::label(i) },
                "metric": { "anyOf": metrics },
                "group_by": { "anyOf": groups },
                "filters": filter_schema,
                "order": { "enum": ["desc", "asc"] }
            },
            "required": ["answerable", "table", "metric", "group_by", "filters", "order"],
            "additionalProperties": false
        }));
    }
    json!({ "anyOf": variants })
}

// ─── The validated plan ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

impl Cmp {
    fn parse(op: &str) -> Option<Self> {
        Some(match op {
            "=" | "==" => Cmp::Eq,
            "!=" | "<>" => Cmp::Ne,
            ">" => Cmp::Gt,
            ">=" => Cmp::Ge,
            "<" => Cmp::Lt,
            "<=" => Cmp::Le,
            _ => return None,
        })
    }

    fn sql(self) -> &'static str {
        match self {
            Cmp::Eq => "=",
            Cmp::Ne => "<>",
            Cmp::Gt => ">",
            Cmp::Ge => ">=",
            Cmp::Lt => "<",
            Cmp::Le => "<=",
        }
    }

    /// The operator as the planner writes it.
    fn json(self) -> &'static str {
        match self {
            Cmp::Eq => "=",
            Cmp::Ne => "!=",
            Cmp::Gt => ">",
            Cmp::Ge => ">=",
            Cmp::Lt => "<",
            Cmp::Le => "<=",
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Cmp::Eq => "=",
            Cmp::Ne => "≠",
            Cmp::Gt => ">",
            Cmp::Ge => "≥",
            Cmp::Lt => "<",
            Cmp::Le => "≤",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    /// Case-insensitive substring.
    Contains { column: usize, value: String, negate: bool },
    /// `value` is a plain decimal, compared as numeric.
    Number { column: usize, op: Cmp, value: String },
    /// `value` is a date prefix (YYYY, YYYY-MM or YYYY-MM-DD), compared with
    /// the same-length prefix of each date: "= 2021" is the whole year,
    /// "> 2021" starts in 2022.
    Date { column: usize, op: Cmp, value: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Year,
    Month,
    Day,
}

impl Period {
    fn prefix_len(self) -> i32 {
        match self {
            Period::Year => 4,
            Period::Month => 7,
            Period::Day => 10,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Func {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Index into the candidates the plan was made for.
    pub table:      usize,
    pub filters:    Vec<Filter>,
    pub group_by:   Option<(usize, Option<Period>)>,
    pub func:       Func,
    /// None for count.
    pub column:     Option<usize>,
    pub descending: bool,
}

#[derive(Deserialize)]
struct RawPlan {
    answerable: bool,
    table:      Option<String>,
    #[serde(default)]
    filters:    Vec<RawFilter>,
    group_by:   Option<RawGroup>,
    metric:     Option<RawMetric>,
    order:      Option<String>,
}

#[derive(Deserialize)]
struct RawFilter {
    column: String,
    op:     String,
    value:  Value,
}

#[derive(Deserialize)]
struct RawGroup {
    column: String,
    period: Option<String>,
}

#[derive(Deserialize)]
struct RawMetric {
    #[serde(rename = "fn")]
    func:   String,
    column: Option<String>,
}

/// "2021", "2021-03", "2021-03-15", or a full date in the Russian format.
fn date_prefix(value: &str) -> Option<String> {
    let value = value.trim();
    if let Some(iso) = parse_date(value) {
        return Some(iso);
    }
    let b = value.as_bytes();
    let year = b.len() >= 4 && b[..4].iter().all(u8::is_ascii_digit);
    match b.len() {
        4 if year => Some(value.to_string()),
        7 if year && b[4] == b'-' && b[5..].iter().all(u8::is_ascii_digit) => Some(value.to_string()),
        _ => None,
    }
}

/// Two different "=" on the same number or date column match no row, so
/// they cannot be what the question meant. A small model writes "during
/// 2021" as `= 2021-01-01` and `= 2021-12-31`: read such a pair as the
/// range between them (inclusive), which is the only sensible reading.
fn equalities_as_range(filters: Vec<Filter>) -> Vec<Filter> {
    let key = |f: &Filter| match f {
        Filter::Number { column, op: Cmp::Eq, .. } => Some((0, *column)),
        Filter::Date { column, op: Cmp::Eq, .. } => Some((1, *column)),
        _ => None,
    };
    let mut out: Vec<Filter> = Vec::new();
    for f in &filters {
        let Some(k) = key(f) else {
            out.push(f.clone());
            continue;
        };
        let same: Vec<&Filter> = filters.iter().filter(|g| key(g) == Some(k)).collect();
        if same.len() != 2 {
            out.push(f.clone());
            continue;
        }
        if !std::ptr::eq(same[0], f) {
            continue; // the pair was handled at its first filter
        }
        let (a, b) = (same[0].clone(), same[1].clone());
        match (a, b) {
            (Filter::Date { column, value: x, .. }, Filter::Date { value: y, .. }) if x != y => {
                let (lo, hi) = if x < y { (x, y) } else { (y, x) };
                out.push(Filter::Date { column, op: Cmp::Ge, value: lo });
                out.push(Filter::Date { column, op: Cmp::Le, value: hi });
            }
            (Filter::Number { column, value: x, .. }, Filter::Number { value: y, .. }) if x != y => {
                let (xf, yf) = (x.parse::<f64>().unwrap_or(0.0), y.parse::<f64>().unwrap_or(0.0));
                let (lo, hi) = if xf < yf { (x, y) } else { (y, x) };
                out.push(Filter::Number { column, op: Cmp::Ge, value: lo });
                out.push(Filter::Number { column, op: Cmp::Le, value: hi });
            }
            (a, b) => {
                out.push(a);
                out.push(b);
            }
        }
    }
    out
}

/// The model's answer, checked against the tables it was shown. `Ok(None)`:
/// the model says no calculation answers the question. `Err`: the plan is
/// unusable — the caller falls back to retrieval either way.
pub fn parse_plan(answer: &str, candidates: &[Candidate]) -> Result<Option<Plan>> {
    let raw: RawPlan = serde_json::from_str(answer.trim()).context("planner answer is not the expected JSON")?;
    if !raw.answerable {
        return Ok(None);
    }
    let label = raw.table.context("plan names no table")?;
    let table = (0..candidates.len())
        .find(|i| Candidate::label(*i) == label)
        .with_context(|| format!("plan names unknown table {label}"))?;
    let columns = &candidates[table].columns;
    let column = |name: &str| -> Result<(usize, ColumnType)> {
        columns
            .iter()
            .position(|c| c.name == name)
            .map(|i| (i, columns[i].kind))
            .with_context(|| format!("no column «{name}» in {label}"))
    };

    anyhow::ensure!(raw.filters.len() <= MAX_FILTERS, "too many filters");
    let mut filters = Vec::new();
    for f in raw.filters {
        let (index, kind) = column(&f.column)?;
        // Numbers may come as JSON numbers from a lenient model.
        let value = match &f.value {
            Value::String(s) => s.trim().to_string(),
            Value::Number(n) => n.to_string(),
            other => bail!("filter value {other} is not a string"),
        };
        anyhow::ensure!(!value.is_empty(), "empty filter value for «{}»", f.column);
        filters.push(match (kind, f.op.as_str()) {
            (ColumnType::Text, "contains") => Filter::Contains { column: index, value, negate: false },
            (ColumnType::Text, "not_contains") => Filter::Contains { column: index, value, negate: true },
            (ColumnType::Number, op) => Filter::Number {
                column: index,
                op:     Cmp::parse(op).with_context(|| format!("operation {op} does not apply to a number"))?,
                value:  parse_number(&value)
                    .map(crate::ingest::extract::format_number)
                    .with_context(|| format!("«{value}» is not a number"))?,
            },
            (ColumnType::Date, op) => Filter::Date {
                column: index,
                op:     Cmp::parse(op).with_context(|| format!("operation {op} does not apply to a date"))?,
                value:  date_prefix(&value).with_context(|| format!("«{value}» is not a date"))?,
            },
            (ColumnType::Text, op) => bail!("operation {op} does not apply to text"),
        });
    }
    let filters = equalities_as_range(filters);

    let group_by = match raw.group_by {
        None => None,
        Some(g) => {
            let (index, kind) = column(&g.column)?;
            let period = match (kind, g.period.as_deref()) {
                (ColumnType::Date, Some("year")) => Some(Period::Year),
                (ColumnType::Date, Some("month")) => Some(Period::Month),
                (ColumnType::Date, Some("day") | None) => Some(Period::Day),
                (_, None) => None,
                (_, Some(p)) => bail!("period {p} applies to date columns only"),
            };
            Some((index, period))
        }
    };

    let metric = raw.metric.context("plan has no metric")?;
    let func = match metric.func.as_str() {
        "count" => Func::Count,
        "sum" => Func::Sum,
        "avg" => Func::Avg,
        "min" => Func::Min,
        "max" => Func::Max,
        other => bail!("unknown metric {other}"),
    };
    let column = match (func, metric.column.as_deref()) {
        (Func::Count, _) => None,
        (_, None) => bail!("metric {} needs a column", metric.func),
        (_, Some(name)) => {
            let (index, kind) = column(name)?;
            match (func, kind) {
                (Func::Sum | Func::Avg, ColumnType::Number) | (Func::Min | Func::Max, ColumnType::Number | ColumnType::Date) => {}
                _ => bail!("metric {} does not apply to «{name}»", metric.func),
            }
            Some(index)
        }
    };

    Ok(Some(Plan {
        table,
        filters,
        group_by,
        func,
        column,
        descending: raw.order.as_deref() != Some("asc"),
    }))
}

// ─── Asking back ─────────────────────────────────────────────────────────────

/// A text column with more distinct values than this is not offered as a
/// grouping: one group per contract number is not an answer.
const MAX_GROUPABLE_DISTINCT: i64 = 1000;

/// Options offered in one clarification, the model's own choice first.
const MAX_OPTIONS: usize = 5;

/// A choice the user makes before the calculation runs: the plan's column
/// was the model's guess, not something the question says.
#[derive(Debug, Clone, Serialize)]
pub struct Clarification {
    pub question: String,
    pub table_id: Uuid,
    pub options:  Vec<ClarifyOption>,
}

/// One answer to a clarification: a complete plan, sent back as is with
/// the question. The core checks it like any planner output (`parse_plan`
/// against the table as this user sees it) — it is data, not trusted.
#[derive(Debug, Clone, Serialize)]
pub struct ClarifyOption {
    pub label: String,
    pub plan:  Value,
}

/// Lowercased words of `text`, letters and digits only.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect()
}

/// Whether the question names `phrase` — any of its words of three or more
/// letters, allowing for Russian endings: "статус" is named by "статусу",
/// "Ответственный" by "ответственного". A crude stem (the word minus two
/// letters, at least four), which is enough to tell "фамилия" from
/// "Контрагент".
fn mentions(question_words: &[String], phrase: &str) -> bool {
    words(phrase).iter().filter(|w| w.chars().count() >= 3).any(|w| {
        let len = w.chars().count();
        let stem: String = if len > 4 { w.chars().take((len - 2).max(4)).collect() } else { w.clone() };
        question_words.iter().any(|q| q.starts_with(&stem))
    })
}

impl Plan {
    /// The plan in the planner's JSON form, for table "T1" — what a
    /// clarification option carries and `parse_plan` reads back.
    pub fn to_json(&self, candidate: &Candidate) -> Value {
        let name = |i: usize| candidate.columns[i].name.clone();
        let filters: Vec<Value> = self
            .filters
            .iter()
            .map(|f| match f {
                Filter::Contains { column, value, negate } => json!({
                    "column": name(*column),
                    "op": if *negate { "not_contains" } else { "contains" },
                    "value": value
                }),
                Filter::Number { column, op, value } | Filter::Date { column, op, value } => {
                    json!({ "column": name(*column), "op": op.json(), "value": value })
                }
            })
            .collect();
        let group_by = match self.group_by {
            None => Value::Null,
            Some((col, None)) => json!({ "column": name(col) }),
            Some((col, Some(p))) => json!({
                "column": name(col),
                "period": match p { Period::Year => "year", Period::Month => "month", Period::Day => "day" }
            }),
        };
        let func = match self.func {
            Func::Count => "count",
            Func::Sum => "sum",
            Func::Avg => "avg",
            Func::Min => "min",
            Func::Max => "max",
        };
        json!({
            "answerable": true,
            "table": "T1",
            "metric": { "fn": func, "column": self.column.map(name) },
            "group_by": group_by,
            "filters": filters,
            "order": if self.descending { "desc" } else { "asc" }
        })
    }
}

/// The question names `phrase` word for word: every word of three or more
/// letters, with the same allowance for endings as `mentions`. Stricter,
/// for the check below — "Номер договора" is not named by a question that
/// says "договоров" (every question here does), only by one that says
/// "номер договора".
fn names_all(question_words: &[String], phrase: &str) -> bool {
    let significant: Vec<String> = words(phrase).into_iter().filter(|w| w.chars().count() >= 3).collect();
    !significant.is_empty()
        && significant.iter().all(|w| {
            let len = w.chars().count();
            let stem: String = if len > 4 { w.chars().take((len - 2).max(4)).collect() } else { w.clone() };
            question_words.iter().any(|q| q.starts_with(&stem))
        })
}

/// "Answer from the documents instead": the plan the core reads as "no
/// calculation" (ADR-0023) — offered in every clarification, since the
/// question may not be a calculation at all.
fn text_only() -> ClarifyOption {
    ClarifyOption { label: "Искать в документах".into(), plan: json!({ "answerable": false }) }
}

/// Whether the plan rests on a guess, or ignores part of the question —
/// then the user chooses instead of the model. Deterministic, in the core:
/// a small model cannot be relied on to notice either. Checked, in order:
///
/// 1. the grouping column is not named (dates grouped by a period
///    excepted: "по годам" has one reading);
/// 2. the metric's column is not named, and the table has others of its
///    type;
/// 3. the question names a value of a column the plan does not filter or
///    group by — "расторгнутых" with no condition on «Статус» (Qwen3-4B
///    dropped exactly that condition, ADR-0024);
/// 4. the question names a column, word for word, that the plan does not
///    use at all — "предметом" with a plain row count (Qwen3-4B planned a
///    calculation for "что является предметом договоров").
///
/// A chosen option comes back checked again (`clarification_after`), so a
/// second problem is not hidden behind the first. Every option records the
/// check it answers in its plan's `confirmed` list — "Не учитывать" must
/// not bring the same question back.
pub fn clarification(plan: &Plan, candidates: &[Candidate], question: &str) -> Option<Clarification> {
    clarification_after(plan, candidates, question, &[])
}

/// `clarification` for a plan the user chose: checks listed in `confirmed`
/// are settled and skipped.
pub fn clarification_after(plan: &Plan, candidates: &[Candidate], question: &str, confirmed: &[String]) -> Option<Clarification> {
    let candidate = candidates.get(plan.table)?;
    let columns = &candidate.columns;
    let q = words(question);
    let named = |i: usize| {
        mentions(&q, &columns[i].name) || columns[i].top.iter().any(|(value, _)| mentions(&q, value))
    };
    let settled = |key: &str| confirmed.iter().any(|c| c == key);
    let option = |label: String, plan: Plan, key: &str| {
        let mut json = plan.to_json(candidate);
        let mut done = confirmed.to_vec();
        done.push(key.to_string());
        json["confirmed"] = json!(done);
        ClarifyOption { label, plan: json }
    };
    let ask = |question: String, mut options: Vec<ClarifyOption>| {
        options.push(text_only());
        Some(Clarification { question, table_id: candidate.table_id, options })
    };
    let groupable = |i: usize| {
        let c = &columns[i];
        c.kind == ColumnType::Text && c.distinct >= 2 && c.distinct <= MAX_GROUPABLE_DISTINCT
    };

    if let Some((col, None)) = plan.group_by {
        if !named(col) && !settled("group") {
            let mut choices: Vec<usize> = vec![col];
            choices.extend((0..columns.len()).filter(|&i| i != col && groupable(i)));
            choices.truncate(MAX_OPTIONS);
            if choices.len() > 1 {
                let mut options: Vec<ClarifyOption> = choices
                    .into_iter()
                    .map(|i| option(columns[i].name.clone(), Plan { group_by: Some((i, None)), ..plan.clone() }, "group"))
                    .collect();
                options.push(option("Без разбивки".into(), Plan { group_by: None, ..plan.clone() }, "group"));
                return ask("Уточните, по какому столбцу разбить результат:".into(), options);
            }
        }
    }

    if let (Some(col), Func::Sum | Func::Avg | Func::Min | Func::Max) = (plan.column, plan.func) {
        // Only columns of the same type: "самый ранний" is a date question,
        // and a sum over a date column is not an alternative to anything.
        let fits = |i: usize| columns[i].kind == columns[col].kind;
        if !named(col) && !settled("metric") {
            let mut choices: Vec<usize> = vec![col];
            choices.extend((0..columns.len()).filter(|&i| i != col && fits(i)));
            choices.truncate(MAX_OPTIONS);
            if choices.len() > 1 {
                let options = choices
                    .into_iter()
                    .map(|i| option(columns[i].name.clone(), Plan { column: Some(i), ..plan.clone() }, "metric"))
                    .collect();
                return ask("Уточните, какой столбец считать:".into(), options);
            }
        }
    }

    let filtered = |i: usize| {
        plan.filters.iter().any(|f| match f {
            Filter::Contains { column, .. } | Filter::Number { column, .. } | Filter::Date { column, .. } => *column == i,
        })
    };
    let grouped = |i: usize| plan.group_by.is_some_and(|(g, _)| g == i);
    let used = |i: usize| filtered(i) || grouped(i) || plan.column == Some(i);

    for (i, c) in columns.iter().enumerate() {
        let key = format!("value:{}", c.name);
        if c.kind != ColumnType::Text || filtered(i) || grouped(i) || settled(&key) {
            continue;
        }
        // Closed columns only: their values are all known, so a match is a
        // real value, not a word that happens to occur in some cell.
        let Some(values) = c.closed_values() else { continue };
        if let Some(value) = values.iter().find(|v| names_all(&q, v)) {
            // The model may have put the value on another column instead
            // ("Кузнецова" on «Контрагент» — observed): that condition moves
            // here rather than staying beside the right one, which would
            // match nothing.
            let misplaced = |f: &Filter| match f {
                Filter::Contains { column, value: v, negate: false } => *column != i && mentions(&words(v), value),
                _ => false,
            };
            let with = Plan {
                filters: {
                    let mut f: Vec<Filter> = plan.filters.iter().filter(|f| !misplaced(f)).cloned().collect();
                    f.push(Filter::Contains { column: i, value: value.clone(), negate: false });
                    f
                },
                ..plan.clone()
            };
            let options = vec![
                option(format!("Учесть: «{}» = {value}", c.name), with, &key),
                option("Не учитывать".into(), plan.clone(), &key),
            ];
            return ask(format!("В вопросе есть «{value}» («{}»), но расчёт это не учитывает:", c.name), options);
        }
    }

    // A year named as a period ("в 2023 году", "за 2023") but a date
    // condition open on one side ("≥ 2023" and nothing above): 2023 and
    // every later year (Qwen3-4B, observed — 1 667 instead of 834).
    let named_years: Vec<String> = q
        .iter()
        .enumerate()
        .filter(|(k, w)| {
            w.len() == 4
                && w.chars().all(|c| c.is_ascii_digit())
                && (w.starts_with("19") || w.starts_with("20"))
                && (q.get(k + 1).is_some_and(|n| n.starts_with("год"))
                    || (*k > 0 && matches!(q[k - 1].as_str(), "в" | "за" | "во")))
        })
        .map(|(_, w)| w.clone())
        .collect();
    for year in &named_years {
        for (i, c) in columns.iter().enumerate() {
            if c.kind != ColumnType::Date {
                continue;
            }
            let on_column: Vec<(Cmp, &String)> = plan
                .filters
                .iter()
                .filter_map(|f| match f {
                    Filter::Date { column, op, value } if *column == i => Some((*op, value)),
                    _ => None,
                })
                .collect();
            let mentions_year = on_column.iter().any(|(_, v)| v.starts_with(year.as_str()));
            let lower = on_column.iter().any(|(op, _)| matches!(op, Cmp::Ge | Cmp::Gt));
            let upper = on_column.iter().any(|(op, _)| matches!(op, Cmp::Le | Cmp::Lt | Cmp::Eq));
            let exact = on_column.iter().any(|(op, _)| *op == Cmp::Eq);
            let key = format!("year:{}:{year}", c.name);
            if mentions_year && !exact && (lower != upper) && !settled(&key) {
                let only_year = Plan {
                    filters: plan
                        .filters
                        .iter()
                        .filter(|f| !matches!(f, Filter::Date { column, .. } if *column == i))
                        .cloned()
                        .chain(std::iter::once(Filter::Date { column: i, op: Cmp::Eq, value: year.clone() }))
                        .collect(),
                    ..plan.clone()
                };
                let open = if lower { format!("С {year} и позже") } else { format!("По {year} включительно") };
                let options = vec![option(format!("Только {year} год"), only_year, &key), option(open, plan.clone(), &key)];
                return ask(format!("Условие по «{}» не ограничено {year} годом:", c.name), options);
            }
        }
    }

    for (i, c) in columns.iter().enumerate() {
        let key = format!("column:{}", c.name);
        if used(i) || !names_all(&q, &c.name) || settled(&key) {
            continue;
        }
        let mut options = Vec::new();
        if groupable(i) {
            options.push(option(format!("Разбить по «{}»", c.name), Plan { group_by: Some((i, None)), ..plan.clone() }, &key));
        }
        options.push(option(format!("Считать без «{}»", c.name), plan.clone(), &key));
        return ask(format!("В вопросе упомянут столбец «{}», но расчёт его не использует:", c.name), options);
    }
    None
}

// ─── Execution ───────────────────────────────────────────────────────────────

enum Param {
    Uuid(Uuid),
    Int(i32),
    Text(String),
}

struct Sql {
    params: Vec<Param>,
}

impl Sql {
    /// Add a parameter, return its placeholder.
    fn bind(&mut self, p: Param) -> String {
        self.params.push(p);
        format!("${}", self.params.len())
    }

    /// Cell `column` of row `r` as text.
    fn cell_text(&mut self, column: usize) -> String {
        format!("(r.cells ->> {}::int)", self.bind(Param::Int(column as i32)))
    }

    /// Cell `column` as numeric, NULL unless it holds a number.
    fn cell_number(&mut self, column: usize) -> String {
        let p = self.bind(Param::Int(column as i32));
        format!("(CASE WHEN jsonb_typeof(r.cells -> {p}::int) = 'number' THEN (r.cells ->> {p}::int)::numeric END)")
    }

    /// Cell `column` as an ISO date string, NULL unless it holds one.
    fn cell_date(&mut self, column: usize) -> String {
        let p = self.bind(Param::Int(column as i32));
        format!("(CASE WHEN (r.cells ->> {p}::int) ~ '^\\d{{4}}-\\d{{2}}-\\d{{2}}' THEN (r.cells ->> {p}::int) END)")
    }
}

/// One line of the result.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// The group's value; None without grouping, or for rows where the
    /// grouping cell is empty.
    pub key:   Option<String>,
    /// The metric; None when no row had a value for it.
    pub value: Option<String>,
    /// Rows in the group.
    pub rows:  i64,
}

/// The result of a plan, with everything needed to explain it.
#[derive(Debug, Clone)]
pub struct Computation {
    pub candidate:    Candidate,
    pub plan:         Plan,
    /// Rows that met the filters.
    pub matched_rows: i64,
    pub groups:       Vec<Group>,
    /// Groups in total (only MAX_GROUPS are listed).
    pub total_groups: i64,
}

/// Build the aggregate for `plan` over `candidate`'s rows.
fn compile(plan: &Plan, candidate: &Candidate) -> (String, Vec<Param>) {
    let mut sql = Sql { params: Vec::new() };
    let table = sql.bind(Param::Uuid(candidate.table_id));

    let mut conditions = vec![format!("r.table_id = {table}")];
    for f in &plan.filters {
        conditions.push(match f {
            Filter::Contains { column, value, negate } => {
                let cell = sql.cell_text(*column);
                // lower() on both sides: one case mapping, the database's.
                let v = sql.bind(Param::Text(value.clone()));
                format!("strpos(lower(coalesce({cell}, '')), lower({v})) {} 0", if *negate { "=" } else { ">" })
            }
            Filter::Number { column, op, value } => {
                let cell = sql.cell_number(*column);
                let v = sql.bind(Param::Text(value.clone()));
                format!("{cell} {} {v}::numeric", op.sql())
            }
            Filter::Date { column, op, value } => {
                let cell = sql.cell_date(*column);
                let len = sql.bind(Param::Int(value.chars().count() as i32));
                let v = sql.bind(Param::Text(value.clone()));
                format!("left({cell}, {len}::int) {} {v}", op.sql())
            }
        });
    }

    let metric = match (plan.func, plan.column) {
        (Func::Count, _) | (_, None) => "count(*)::text".to_string(),
        (func, Some(col)) => {
            let cell = if candidate.columns[col].kind == ColumnType::Date { sql.cell_date(col) } else { sql.cell_number(col) };
            match func {
                Func::Sum => format!("sum({cell})::text"),
                Func::Avg => format!("round(avg({cell}), 2)::text"),
                Func::Min => format!("min({cell})::text"),
                Func::Max => format!("max({cell})::text"),
                Func::Count => unreachable!(),
            }
        }
    };

    let group = plan.group_by.map(|(col, period)| match period {
        Some(p) => {
            let cell = sql.cell_date(col);
            let len = sql.bind(Param::Int(p.prefix_len()));
            format!("left({cell}, {len}::int)")
        }
        None => sql.cell_text(col),
    });

    let where_clause = conditions.join(" AND ");
    let text = match group {
        None => format!(
            "SELECT NULL::text AS grp, {metric} AS val, count(*) AS n, count(*) AS matched, 1::bigint AS groups \
             FROM sheet_rows r WHERE {where_clause}"
        ),
        Some(g) => {
            // Rank groups by the metric's own type (numeric or date text):
            // ordering by its ::text form would put "9" above "10".
            let rank = metric.trim_end_matches("::text");
            let direction = if plan.descending { "DESC" } else { "ASC" };
            let limit = sql.bind(Param::Int(MAX_GROUPS as i32));
            format!(
                "SELECT {g} AS grp, {metric} AS val, count(*) AS n, \
                        sum(count(*)) OVER ()::bigint AS matched, count(*) OVER () AS groups \
                 FROM sheet_rows r WHERE {where_clause} \
                 GROUP BY 1 ORDER BY {rank} {direction} NULLS LAST, 1 LIMIT {limit}::int"
            )
        }
    };
    (text, sql.params)
}

/// Run `plan` in the caller's transaction (app_pool, identity set): RLS
/// decides which rows the aggregate sees.
pub async fn execute(conn: &mut PgConnection, candidates: &[Candidate], plan: Plan) -> Result<Computation> {
    let candidate = candidates.get(plan.table).context("plan refers to no candidate")?.clone();
    let (text, params) = compile(&plan, &candidate);
    let mut query = sqlx::query(&text);
    for p in params {
        query = match p {
            Param::Uuid(v) => query.bind(v),
            Param::Int(v) => query.bind(v),
            Param::Text(v) => query.bind(v),
        };
    }
    let rows = query.fetch_all(&mut *conn).await?;

    let matched_rows = rows.first().map(|r| r.try_get::<i64, _>("matched")).transpose()?.unwrap_or(0);
    let total_groups = rows.first().map(|r| r.try_get::<i64, _>("groups")).transpose()?.unwrap_or(0);
    let groups = rows
        .iter()
        .map(|r| {
            Ok(Group {
                key:   r.try_get("grp")?,
                value: r.try_get("val")?,
                rows:  r.try_get("n")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Computation { candidate, plan, matched_rows, groups, total_groups })
}

// ─── Explaining the result ───────────────────────────────────────────────────

/// "1234567.50" → "1 234 567.5": digits grouped for reading, trailing
/// zeros from numeric division dropped. Dates and text pass through.
fn readable(value: &str) -> String {
    let Some(f) = parse_number(value) else { return value.to_string() };
    let plain = crate::ingest::extract::format_number(f);
    let (sign, digits) = plain.strip_prefix('-').map(|d| ("-", d)).unwrap_or(("", plain.as_str()));
    let (whole, fraction) = digits.split_once('.').map(|(w, f)| (w, Some(f))).unwrap_or((digits, None));
    let mut grouped = String::new();
    for (i, c) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            grouped.push(' ');
        }
        grouped.push(c);
    }
    match fraction {
        Some(f) => format!("{sign}{grouped}.{f}"),
        None => format!("{sign}{grouped}"),
    }
}

impl Computation {
    fn column_name(&self, index: usize) -> &str {
        &self.candidate.columns[index].name
    }

    /// What was computed, in words: shown to the user under the answer and
    /// given to the model as its only source.
    pub fn describe(&self) -> String {
        let plan = &self.plan;
        let mut out = format!(
            "Расчёт по таблице: файл «{}», лист «{}» ({} строк).\n",
            self.candidate.filename,
            self.candidate.sheet,
            readable(&self.candidate.row_count.to_string())
        );

        if plan.filters.is_empty() {
            out.push_str("Условия: нет, все строки.\n");
        } else {
            let conditions: Vec<String> = plan
                .filters
                .iter()
                .map(|f| match f {
                    Filter::Contains { column, value, negate } => format!(
                        "«{}» {} «{value}»",
                        self.column_name(*column),
                        if *negate { "не содержит" } else { "содержит" }
                    ),
                    Filter::Number { column, op, value } => {
                        format!("«{}» {} {}", self.column_name(*column), op.symbol(), readable(value))
                    }
                    Filter::Date { column, op, value } => {
                        format!("«{}» {} {value}", self.column_name(*column), op.symbol())
                    }
                })
                .collect();
            out.push_str(&format!("Условия: {}.\n", conditions.join("; ")));
        }
        out.push_str(&format!("Подошло строк: {}.\n", readable(&self.matched_rows.to_string())));
        out.push_str(&self.result_text());
        out
    }

    /// The answer itself when the result is split into groups: the list,
    /// written by the core. A small model restating a list of groups merges
    /// them, drops their numbers or pairs a name with the grand total
    /// ("50 000, фамилия: Иванов И.И." — observed); the list needs no
    /// rephrasing, so the model is not asked. None for a single result,
    /// which the model states well.
    pub fn grouped_answer(&self) -> Option<String> {
        self.plan.group_by.map(|_| self.result_text())
    }

    /// The result lines: the metric, one line per group when grouped.
    fn result_text(&self) -> String {
        let plan = &self.plan;
        let mut out = String::new();
        let metric = match (plan.func, plan.column) {
            (Func::Count, _) | (_, None) => "Количество строк".to_string(),
            (Func::Sum, Some(c)) => format!("Сумма «{}»", self.column_name(c)),
            (Func::Avg, Some(c)) => format!("Среднее «{}»", self.column_name(c)),
            (Func::Min, Some(c)) => format!("Минимум «{}»", self.column_name(c)),
            (Func::Max, Some(c)) => format!("Максимум «{}»", self.column_name(c)),
        };
        let value = |v: &Option<String>| v.as_deref().map(readable).unwrap_or_else(|| "нет значений".to_string());

        match plan.group_by {
            None => {
                let v = self.groups.first().map(|g| value(&g.value)).unwrap_or_else(|| "нет значений".to_string());
                out.push_str(&format!("{metric}: {v}."));
            }
            Some((col, period)) => {
                let by = match period {
                    Some(Period::Year) => " (по годам)",
                    Some(Period::Month) => " (по месяцам)",
                    Some(Period::Day) => " (по дням)",
                    None => "",
                };
                out.push_str(&format!("{metric} по «{}»{by}:\n", self.column_name(col)));
                for g in &self.groups {
                    let key = g.key.as_deref().unwrap_or("(пусто)");
                    let rows = if matches!(plan.func, Func::Count) {
                        String::new()
                    } else {
                        format!(" (строк: {})", readable(&g.rows.to_string()))
                    };
                    out.push_str(&format!("- {key}: {}{rows}\n", value(&g.value)));
                }
                let hidden = self.total_groups - self.groups.len() as i64;
                if hidden > 0 {
                    out.push_str(&format!("Ещё групп, не показанных здесь: {hidden}.\n"));
                }
                if self.groups.is_empty() {
                    out.push_str("Групп нет.\n");
                }
            }
        }
        out.trim_end().to_string()
    }
}

/// System and user messages for the answer: the computation is the only
/// source, and its numbers are final.
pub fn answer_messages(computation: &Computation, question: &str) -> (&'static str, String) {
    let system = "You answer questions about the organization's documents. \
                  The source is the exact result of a calculation the application ran over a spreadsheet. \
                  Report its numbers exactly as written there — never recalculate, round or convert them — \
                  and say which conditions the calculation used. Cite it as [Source 1]. \
                  If the result is split into groups, list every group with its own number, \
                  one per line, exactly as in the source; never merge them into one total. \
                  If no rows matched, say so and name the conditions. \
                  Answer once, concisely, in the language of the question.";
    (
        system,
        format!(
            "Sources:\n\n[Source 1] {}\n{}\n\nQuestion: {question}",
            computation.candidate.filename,
            computation.describe()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, kind: ColumnType) -> Column {
        Column { name: name.into(), kind, distinct: 3, top: vec![], min: None, max: None }
    }

    fn registry() -> Vec<Candidate> {
        vec![Candidate {
            table_id:    Uuid::nil(),
            document_id: Uuid::nil(),
            filename:    "Реестр.xlsx".into(),
            sheet:       "Реестр".into(),
            row_count:   50000,
            columns:     vec![
                col("Номер договора", ColumnType::Text),
                col("Сумма, руб.", ColumnType::Number),
                col("Дата подписания", ColumnType::Date),
                col("Ответственный", ColumnType::Text),
            ],
        }]
    }

    #[test]
    fn a_valid_plan_is_parsed_and_typed() {
        let answer = r#"{"answerable": true, "table": "T1",
            "filters": [{"column": "Ответственный", "op": "contains", "value": "Петрова"},
                        {"column": "Дата подписания", "op": "=", "value": "2021"},
                        {"column": "Сумма, руб.", "op": ">=", "value": "1 000 000,5"}],
            "group_by": null, "metric": {"fn": "sum", "column": "Сумма, руб."}, "order": "desc"}"#;
        let plan = parse_plan(answer, &registry()).unwrap().unwrap();
        assert_eq!(
            plan.filters,
            [
                Filter::Contains { column: 3, value: "Петрова".into(), negate: false },
                Filter::Date { column: 2, op: Cmp::Eq, value: "2021".into() },
                Filter::Number { column: 1, op: Cmp::Ge, value: "1000000.5".into() },
            ]
        );
        assert_eq!((plan.func, plan.column, plan.group_by), (Func::Sum, Some(1), None));
    }

    #[test]
    fn two_equalities_on_one_column_are_read_as_a_range() {
        let answer = r#"{"answerable": true, "table": "T1", "metric": {"fn": "count", "column": null}, "group_by": null,
            "filters": [{"column": "Дата подписания", "op": "=", "value": "2021-12-31"},
                        {"column": "Ответственный", "op": "contains", "value": "Петрова"},
                        {"column": "Дата подписания", "op": "=", "value": "2021-01-01"}], "order": "desc"}"#;
        let plan = parse_plan(answer, &registry()).unwrap().unwrap();
        assert_eq!(
            plan.filters,
            [
                Filter::Date { column: 2, op: Cmp::Ge, value: "2021-01-01".into() },
                Filter::Date { column: 2, op: Cmp::Le, value: "2021-12-31".into() },
                Filter::Contains { column: 3, value: "Петрова".into(), negate: false },
            ]
        );
    }

    #[test]
    fn not_answerable_is_none() {
        assert_eq!(parse_plan(r#"{"answerable": false}"#, &registry()).unwrap(), None);
    }

    #[test]
    fn plans_that_do_not_fit_the_table_are_rejected() {
        let cases = [
            // Unknown column, unknown table, text summed, number "contains",
            // unparseable number and date, date period on a text column.
            r#"{"answerable": true, "table": "T1", "filters": [], "group_by": null, "metric": {"fn": "sum", "column": "Премия"}, "order": "desc"}"#,
            r#"{"answerable": true, "table": "T2", "filters": [], "group_by": null, "metric": {"fn": "count", "column": null}, "order": "desc"}"#,
            r#"{"answerable": true, "table": "T1", "filters": [], "group_by": null, "metric": {"fn": "sum", "column": "Ответственный"}, "order": "desc"}"#,
            r#"{"answerable": true, "table": "T1", "filters": [{"column": "Сумма, руб.", "op": "contains", "value": "5"}], "group_by": null, "metric": {"fn": "count", "column": null}, "order": "desc"}"#,
            r#"{"answerable": true, "table": "T1", "filters": [{"column": "Сумма, руб.", "op": ">", "value": "много"}], "group_by": null, "metric": {"fn": "count", "column": null}, "order": "desc"}"#,
            r#"{"answerable": true, "table": "T1", "filters": [{"column": "Дата подписания", "op": "=", "value": "прошлый год"}], "group_by": null, "metric": {"fn": "count", "column": null}, "order": "desc"}"#,
            r#"{"answerable": true, "table": "T1", "filters": [], "group_by": {"column": "Ответственный", "period": "year"}, "metric": {"fn": "count", "column": null}, "order": "desc"}"#,
            "not json at all",
        ];
        for case in cases {
            assert!(parse_plan(case, &registry()).is_err(), "must be rejected: {case}");
        }
    }

    #[test]
    fn compiled_sql_carries_values_only_as_parameters() {
        let plan = Plan {
            table:      0,
            filters:    vec![
                Filter::Contains { column: 3, value: "'; DROP TABLE users; --".into(), negate: false },
                Filter::Date { column: 2, op: Cmp::Gt, value: "2021".into() },
            ],
            group_by:   Some((2, Some(Period::Year))),
            func:       Func::Sum,
            column:     Some(1),
            descending: true,
        };
        let (sql, params) = compile(&plan, &registry()[0]);
        assert!(!sql.contains("DROP"), "{sql}");
        assert!(!sql.contains("2021"), "{sql}");
        assert!(sql.contains("GROUP BY 1"), "{sql}");
        assert!(params.len() >= 5);
    }

    #[test]
    fn schema_lists_only_the_tables_columns_by_type() {
        let schema = plan_schema(&registry()).to_string();
        assert!(schema.contains(r#""const":"T1""#), "{schema}");
        assert!(schema.contains("Ответственный"));
        // Sum is offered over number columns only.
        let v = plan_schema(&registry());
        let metrics = &v["anyOf"][1]["properties"]["metric"]["anyOf"];
        assert_eq!(metrics[1]["properties"]["column"]["enum"], json!(["Сумма, руб."]));
        assert_eq!(metrics[2]["properties"]["column"]["enum"], json!(["Дата подписания"]));
    }

    #[test]
    fn closed_text_columns_offer_only_their_values() {
        let mut candidates = registry();
        let status = Column {
            name:     "Статус".into(),
            kind:     ColumnType::Text,
            distinct: 2,
            top:      vec![("исполнен".into(), 5), ("расторгнут".into(), 3)],
            min:      None,
            max:      None,
        };
        candidates[0].columns.push(status);
        let v = plan_schema(&candidates);
        let table = &v["anyOf"][1]["properties"];
        // Fields in the order the model should decide them.
        let order: Vec<&String> = table.as_object().unwrap().keys().collect();
        assert_eq!(order, ["answerable", "table", "metric", "group_by", "filters", "order"]);
        let filters = table["filters"]["items"]["anyOf"].as_array().unwrap();
        let closed = filters.iter().find(|f| f["properties"]["column"]["const"] == "Статус").expect("closed variant");
        assert_eq!(closed["properties"]["value"]["enum"], json!(["исполнен", "расторгнут"]));
        // Open text columns share one free-text variant.
        assert!(filters.iter().any(|f| f["properties"]["column"]["enum"] == json!(["Номер договора", "Ответственный"])));
    }

    fn planned(answer: &str, candidates: &[Candidate]) -> Plan {
        parse_plan(answer, candidates).unwrap().unwrap()
    }

    #[test]
    fn a_grouping_the_question_never_names_is_asked_back() {
        let candidates = registry();
        let by_manager = r#"{"answerable": true, "table": "T1", "metric": {"fn": "count", "column": null},
            "group_by": {"column": "Ответственный"}, "filters": [], "order": "desc"}"#;
        let plan = planned(by_manager, &candidates);

        // Named, in another case — no question.
        assert!(clarification(&plan, &candidates, "Сколько договоров у каждого ответственного?").is_none());

        // "фамилия" is not a column: the user chooses.
        let c = clarification(&plan, &candidates, "кол-во договоров и фамилия").expect("must ask back");
        let labels: Vec<&str> = c.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["Ответственный", "Номер договора", "Без разбивки", "Искать в документах"]);
        // Every option is a plan that parses back to what its label says.
        let chosen = planned(&c.options[1].plan.to_string(), &candidates);
        assert_eq!(chosen.group_by, Some((0, None)));
        assert_eq!(planned(&c.options[2].plan.to_string(), &candidates).group_by, None);
        assert_eq!(planned(&c.options[0].plan.to_string(), &candidates), plan);
    }

    #[test]
    fn values_and_date_periods_count_as_named() {
        let mut candidates = registry();
        candidates[0].columns[3].top = vec![("Иванов И.И.".into(), 5), ("Петрова А.С.".into(), 5)];
        candidates[0].columns[3].distinct = 2;
        let by_manager = planned(
            r#"{"answerable": true, "table": "T1", "metric": {"fn": "count", "column": null},
               "group_by": {"column": "Ответственный"}, "filters": [], "order": "desc"}"#,
            &candidates,
        );
        // A value of the column names it.
        assert!(clarification(&by_manager, &candidates, "сколько договоров у Иванова и Петровой").is_none());
        // Grouping a date by year has one reading.
        let by_year = planned(
            r#"{"answerable": true, "table": "T1", "metric": {"fn": "count", "column": null},
               "group_by": {"column": "Дата подписания", "period": "year"}, "filters": [], "order": "desc"}"#,
            &candidates,
        );
        assert!(clarification(&by_year, &candidates, "сколько договоров по годам").is_none());
    }

    #[test]
    fn a_metric_column_is_asked_back_only_among_its_own_type() {
        let mut candidates = registry();
        candidates[0].columns.push(col("НДС", ColumnType::Number));
        let sum = planned(
            r#"{"answerable": true, "table": "T1", "metric": {"fn": "sum", "column": "Сумма, руб."},
               "group_by": null, "filters": [], "order": "desc"}"#,
            &candidates,
        );
        assert!(clarification(&sum, &candidates, "общая сумма договоров").is_none());
        let c = clarification(&sum, &candidates, "сколько всего денег по договорам").expect("two number columns");
        let labels: Vec<&str> = c.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["Сумма, руб.", "НДС", "Искать в документах"]);
        // The only date column: "earliest" is not ambiguous.
        let earliest = planned(
            r#"{"answerable": true, "table": "T1", "metric": {"fn": "min", "column": "Дата подписания"},
               "group_by": null, "filters": [], "order": "asc"}"#,
            &candidates,
        );
        assert!(clarification(&earliest, &candidates, "когда заключён самый ранний договор").is_none());
    }

    #[test]
    fn a_value_the_question_names_but_the_plan_ignores_is_asked_back() {
        // Qwen3-4B, "Сколько расторгнутых договоров у Сидорова в 2023
        // году?": the manager and the year made it into the plan, the
        // status did not — 2 500 instead of 833.
        let mut candidates = registry();
        candidates[0].columns.push(Column {
            name:     "Статус".into(),
            kind:     ColumnType::Text,
            distinct: 3,
            top:      vec![("исполнен".into(), 5), ("расторгнут".into(), 3), ("действует".into(), 2)],
            min:      None,
            max:      None,
        });
        let dropped = planned(
            r#"{"answerable": true, "table": "T1", "metric": {"fn": "count", "column": null}, "group_by": null,
               "filters": [{"column": "Ответственный", "op": "contains", "value": "Сидоров"}], "order": "desc"}"#,
            &candidates,
        );
        let q = "Сколько расторгнутых договоров у Сидорова в 2023 году?";
        let c = clarification(&dropped, &candidates, q).expect("the status must not be dropped silently");
        let labels: Vec<&str> = c.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["Учесть: «Статус» = расторгнут", "Не учитывать", "Искать в документах"]);
        let with = planned(&c.options[0].plan.to_string(), &candidates);
        assert!(with.filters.contains(&Filter::Contains { column: 4, value: "расторгнут".into(), negate: false }));
        // With the condition in the plan there is nothing to ask.
        assert!(clarification(&with, &candidates, q).is_none());

        // The value on the wrong column (a closed «Ответственный» and the
        // name put on «Номер договора»): "Учесть" moves it, it does not
        // leave it there to match nothing.
        candidates[0].columns[3].top = vec![("Кузнецова Е.В.".into(), 5), ("Сидоров П.П.".into(), 5)];
        candidates[0].columns[3].distinct = 2;
        let misplaced = planned(
            r#"{"answerable": true, "table": "T1", "metric": {"fn": "count", "column": null}, "group_by": null,
               "filters": [{"column": "Номер договора", "op": "contains", "value": "Кузнецова"}], "order": "desc"}"#,
            &candidates,
        );
        let c = clarification(&misplaced, &candidates, "Сколько договоров у Кузнецовой?").expect("must ask");
        let fixed = planned(&c.options[0].plan.to_string(), &candidates);
        assert_eq!(fixed.filters, [Filter::Contains { column: 3, value: "Кузнецова Е.В.".into(), negate: false }]);
    }

    #[test]
    fn a_year_named_as_a_period_with_an_open_date_condition_is_asked_back_once() {
        // Qwen3-4B, "…у Кузнецовой в 2023 году?": "≥ 2023" and no upper
        // bound — 2023 and 2024 together.
        let candidates = registry();
        let open = planned(
            r#"{"answerable": true, "table": "T1", "metric": {"fn": "count", "column": null}, "group_by": null,
               "filters": [{"column": "Дата подписания", "op": ">=", "value": "2023"}], "order": "desc"}"#,
            &candidates,
        );
        let q = "Сколько договоров подписано в 2023 году?";
        let c = clarification(&open, &candidates, q).expect("must ask");
        let labels: Vec<&str> = c.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["Только 2023 год", "С 2023 и позже", "Искать в документах"]);
        assert_eq!(
            planned(&c.options[0].plan.to_string(), &candidates).filters,
            [Filter::Date { column: 2, op: Cmp::Eq, value: "2023".into() }]
        );
        // "С 2023 и позже" is settled: checked again, that plan asks nothing.
        let keep = &c.options[1].plan;
        let confirmed: Vec<String> = serde_json::from_value(keep["confirmed"].clone()).unwrap();
        assert!(clarification_after(&planned(&keep.to_string(), &candidates), &candidates, q, &confirmed).is_none());
        // "после 2022" is not a year named as a period.
        let after = planned(
            r#"{"answerable": true, "table": "T1", "metric": {"fn": "count", "column": null}, "group_by": null,
               "filters": [{"column": "Дата подписания", "op": ">", "value": "2022"}], "order": "desc"}"#,
            &candidates,
        );
        assert!(clarification(&after, &candidates, "Сколько договоров подписано после 2022?").is_none());
    }

    #[test]
    fn a_column_the_question_names_but_the_plan_ignores_is_asked_back() {
        // Qwen3-4B planned a row count for "Что является предметом
        // договоров?" — a question about text.
        let mut candidates = registry();
        candidates[0].columns.push(col("Предмет", ColumnType::Text));
        let count_all = planned(
            r#"{"answerable": true, "table": "T1", "metric": {"fn": "count", "column": null}, "group_by": null, "filters": [], "order": "desc"}"#,
            &candidates,
        );
        let c = clarification(&count_all, &candidates, "Что является предметом договоров?").expect("must ask");
        let labels: Vec<&str> = c.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["Разбить по «Предмет»", "Считать без «Предмет»", "Искать в документах"]);
        assert_eq!(c.options[2].plan, json!({ "answerable": false }));
        // "договоров" alone does not name «Номер договора»: every question
        // here says it.
        assert!(clarification(&count_all, &candidates, "Сколько всего договоров?").is_none());
    }

    #[test]
    fn numbers_are_grouped_for_reading() {
        assert_eq!(readable("1234567.50"), "1 234 567.5");
        assert_eq!(readable("-1000"), "-1 000");
        assert_eq!(readable("999"), "999");
        assert_eq!(readable("2021-03-15"), "2021-03-15");
    }
}
