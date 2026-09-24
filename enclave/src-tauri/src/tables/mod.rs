//! Spreadsheets as typed tables, for calculations over them (ADR-0022).
//!
//! RAG answers questions about a row; it cannot count or sum — "общая сумма
//! договоров Петровой" is in no chunk. At ingestion every sheet is also
//! stored as a table (`sheet_tables` / `sheet_rows`): columns with an
//! inferred type, rows as JSONB arrays. At query time the model turns the
//! question into a plan constrained to those columns (`plan`), and the core
//! compiles the validated plan into a parameterized aggregate that runs on
//! app_pool under RLS. The model never writes SQL.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use std::collections::HashMap;
use uuid::Uuid;

use crate::ingest::extract::{format_number, is_iso_date, CellValue, Sheet};

pub mod plan;

/// Rows per INSERT statement.
const WRITE_BATCH: usize = 5000;

/// A column is a number (or date) column when at least this share of its
/// non-empty cells are numbers (dates) — a stray "н/д" or "—" in a column
/// of amounts should not turn it into text.
const TYPE_SHARE: f64 = 0.9;

/// Most frequent values kept per text column: they show the model how a
/// value is spelled ("Петрова А.С.", not "Петрова"), and a column with no
/// more distinct values than this is "closed" — the planner's grammar lets
/// a filter on it name only those exact values (plan::plan_schema).
pub const TOP_VALUES: usize = 30;

/// Text values longer than this are not worth showing as examples.
const TOP_VALUE_MAX_CHARS: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColumnType {
    Number,
    Date,
    Text,
}

/// A column as stored in `sheet_tables.columns`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub name:     String,
    #[serde(rename = "type")]
    pub kind:     ColumnType,
    /// Non-empty cells with distinct values.
    pub distinct: i64,
    /// Text columns: the most frequent values and how many rows hold each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top:      Vec<(String, i64)>,
    /// Number and date columns: the range of values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min:      Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max:      Option<String>,
}

impl Column {
    /// Every value of a text column, when all of them are known (few
    /// distinct values, none too long to keep) — "Статус", "Отдел".
    pub fn closed_values(&self) -> Option<Vec<String>> {
        (self.kind == ColumnType::Text && self.distinct > 0 && self.distinct as usize == self.top.len())
            .then(|| self.top.iter().map(|(v, _)| v.clone()).collect())
    }
}

/// A sheet ready to be written: typed columns, and each row as the JSON
/// text of its cells array.
#[derive(Debug)]
pub struct TableData {
    pub sheet:   String,
    pub columns: Vec<Column>,
    pub rows:    Vec<(i32, String)>,
}

/// "120 000,50" → 120000.5. Amounts are often typed as text in Russian
/// spreadsheets — spaces between thousands, a decimal comma. Plain digits
/// only: no exponents, currency signs or percentages.
pub fn parse_number(text: &str) -> Option<f64> {
    let cleaned: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let cleaned = if cleaned.contains(',') && !cleaned.contains('.') {
        cleaned.replace(',', ".")
    } else {
        cleaned.replace(',', "")
    };
    let digits = cleaned.strip_prefix('-').unwrap_or(&cleaned);
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let plain = !whole.is_empty()
        && whole.chars().all(|c| c.is_ascii_digit())
        && fraction.chars().all(|c| c.is_ascii_digit())
        && !(digits.contains('.') && fraction.is_empty());
    if !plain {
        return None;
    }
    cleaned.parse::<f64>().ok().filter(|f| f.is_finite())
}

/// "15.03.2021" → "2021-03-15"; an ISO date stays as it is.
pub fn parse_date(text: &str) -> Option<String> {
    let text = text.trim();
    if is_iso_date(text) {
        return Some(text.to_string());
    }
    let mut parts = text.split('.');
    let (d, m, y) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || y.len() != 4 || d.len() > 2 || m.len() > 2 {
        return None;
    }
    let date = chrono::NaiveDate::from_ymd_opt(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)?;
    Some(date.format("%Y-%m-%d").to_string())
}

fn number_of(cell: &CellValue) -> Option<f64> {
    match cell {
        CellValue::Number(f) => Some(*f),
        CellValue::Text(t) => parse_number(t),
        CellValue::Date(_) => None,
    }
}

fn date_of(cell: &CellValue) -> Option<String> {
    match cell {
        CellValue::Date(d) => Some(d.clone()),
        CellValue::Text(t) => parse_date(t),
        CellValue::Number(_) => None,
    }
}

fn display(cell: &CellValue) -> String {
    match cell {
        CellValue::Number(f) => format_number(*f),
        CellValue::Date(s) | CellValue::Text(s) => s.clone(),
    }
}

/// Type each column, convert its cells, and collect what the planner is
/// shown about it.
pub fn build_table(sheet: &Sheet) -> TableData {
    let width = sheet.columns.len();
    let kinds: Vec<ColumnType> = (0..width)
        .map(|j| {
            let cells: Vec<&CellValue> = sheet.rows.iter().filter_map(|r| r.cells.get(j)?.as_ref()).collect();
            let filled = cells.len() as f64;
            if filled == 0.0 {
                ColumnType::Text
            } else if cells.iter().filter(|c| number_of(c).is_some()).count() as f64 >= TYPE_SHARE * filled {
                ColumnType::Number
            } else if cells.iter().filter(|c| date_of(c).is_some()).count() as f64 >= TYPE_SHARE * filled {
                ColumnType::Date
            } else {
                ColumnType::Text
            }
        })
        .collect();

    // Cells as JSON, typed by their column; stats as we go.
    let mut counts: Vec<HashMap<String, i64>> = vec![HashMap::new(); width];
    let mut numeric_range: Vec<Option<(f64, f64)>> = vec![None; width];
    let mut date_range: Vec<Option<(String, String)>> = vec![None; width];
    let mut rows = Vec::with_capacity(sheet.rows.len());

    for row in &sheet.rows {
        let mut json = String::from("[");
        for j in 0..width {
            if j > 0 {
                json.push(',');
            }
            let Some(cell) = row.cells.get(j).and_then(Option::as_ref) else {
                json.push_str("null");
                continue;
            };
            let text = match (kinds[j], number_of(cell), date_of(cell)) {
                (ColumnType::Number, Some(f), _) => {
                    let range = numeric_range[j].get_or_insert((f, f));
                    range.0 = range.0.min(f);
                    range.1 = range.1.max(f);
                    // Written as the cell displays it ("0.3", not
                    // "0.30000000000000004"): Postgres reads JSON numbers
                    // as numeric, so sums stay exact decimals.
                    json.push_str(&format_number(f));
                    *counts[j].entry(format_number(f)).or_default() += 1;
                    continue;
                }
                (ColumnType::Date, _, Some(d)) => {
                    let range = date_range[j].get_or_insert_with(|| (d.clone(), d.clone()));
                    if d < range.0 {
                        range.0 = d.clone();
                    }
                    if d > range.1 {
                        range.1 = d.clone();
                    }
                    d
                }
                _ => display(cell),
            };
            json.push_str(&serde_json::Value::String(text.clone()).to_string());
            *counts[j].entry(text).or_default() += 1;
        }
        json.push(']');
        rows.push((row.number as i32, json));
    }

    let columns = (0..width)
        .map(|j| {
            let (min, max) = match kinds[j] {
                ColumnType::Number => numeric_range[j].map(|(a, b)| (format_number(a), format_number(b))).unzip(),
                ColumnType::Date => date_range[j].clone().unzip(),
                ColumnType::Text => (None, None),
            };
            let top = if kinds[j] == ColumnType::Text {
                let mut values: Vec<(String, i64)> = counts[j]
                    .iter()
                    .filter(|(v, _)| v.chars().count() <= TOP_VALUE_MAX_CHARS)
                    .map(|(v, n)| (v.clone(), *n))
                    .collect();
                values.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                values.truncate(TOP_VALUES);
                values
            } else {
                Vec::new()
            };
            Column {
                name: sheet.columns[j].clone(),
                kind: kinds[j],
                distinct: counts[j].len() as i64,
                top,
                min,
                max,
            }
        })
        .collect();

    TableData { sheet: sheet.name.clone(), columns, rows }
}

/// Write a document's tables inside the worker's write transaction — the
/// same one as its chunks, so a document is ready with both or neither.
/// Runs on ingest_pool (BYPASSRLS): `department_id` is stamped here, from
/// the document row, exactly as for chunks (ADR-0009).
pub async fn write_tables(
    conn:          &mut PgConnection,
    document_id:   Uuid,
    department_id: Uuid,
    tables:        &[TableData],
) -> Result<()> {
    for (index, table) in tables.iter().enumerate() {
        let table_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO sheet_tables (document_id, department_id, sheet, sheet_index, row_count, columns)
            VALUES ($1, $2, $3, $4, $5, $6)
            RETURNING id
            "#,
        )
        .bind(document_id)
        .bind(department_id)
        .bind(&table.sheet)
        .bind(index as i32)
        .bind(table.rows.len() as i32)
        .bind(serde_json::to_value(&table.columns)?)
        .fetch_one(&mut *conn)
        .await?;

        for batch in table.rows.chunks(WRITE_BATCH) {
            let numbers: Vec<i32> = batch.iter().map(|(n, _)| *n).collect();
            let cells: Vec<&str> = batch.iter().map(|(_, c)| c.as_str()).collect();
            sqlx::query(
                r#"
                INSERT INTO sheet_rows (table_id, department_id, row_number, cells)
                SELECT $1, $2, t.n, t.cells::jsonb
                FROM UNNEST($3::int[], $4::text[]) AS t(n, cells)
                "#,
            )
            .bind(table_id)
            .bind(department_id)
            .bind(&numbers)
            .bind(&cells)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::extract::SheetRow;

    #[test]
    fn numbers_typed_as_text_are_read() {
        assert_eq!(parse_number("120 000,50"), Some(120000.5));
        assert_eq!(parse_number("1,234,567.8"), Some(1234567.8));
        assert_eq!(parse_number("-15"), Some(-15.0));
        assert_eq!(parse_number("\u{a0}42\u{a0}"), Some(42.0));
        for not_a_number in ["", "-", "1e5", "15%", "12 руб.", "Д-012345", "inf", "1.", ".5", "1.2.3"] {
            assert_eq!(parse_number(not_a_number), None, "{not_a_number}");
        }
    }

    #[test]
    fn dates_typed_as_text_are_read() {
        assert_eq!(parse_date("15.03.2021").as_deref(), Some("2021-03-15"));
        assert_eq!(parse_date("1.3.2021").as_deref(), Some("2021-03-01"));
        assert_eq!(parse_date("2021-03-15").as_deref(), Some("2021-03-15"));
        assert_eq!(parse_date("31.02.2021"), None);
        assert_eq!(parse_date("15.03.21"), None);
    }

    fn row(number: usize, cells: Vec<Option<CellValue>>) -> SheetRow {
        SheetRow { number, cells }
    }

    #[test]
    fn columns_are_typed_by_most_of_their_cells() {
        use CellValue::*;
        let mut rows: Vec<SheetRow> = (0..20)
            .map(|i| {
                row(
                    i + 2,
                    vec![
                        Some(Text(if i % 2 == 0 { "Петрова А.С." } else { "Иванов" }.into())),
                        Some(Number(1000.0 * i as f64 + 0.1 + 0.2)),
                        Some(Date(format!("2021-03-{:02}", i + 1))),
                        Some(Text(format!("{} 000,5", i + 1))),
                    ],
                )
            })
            .collect();
        // One odd cell per column does not change its type.
        rows.push(row(22, vec![None, Some(Text("н/д".into())), Some(Text("—".into())), None]));
        let sheet = Sheet {
            name:    "Реестр".into(),
            columns: vec!["Ответственный".into(), "Сумма".into(), "Дата".into(), "Сумма текстом".into()],
            rows,
        };
        let table = build_table(&sheet);
        let kinds: Vec<ColumnType> = table.columns.iter().map(|c| c.kind).collect();
        assert_eq!(kinds, [ColumnType::Text, ColumnType::Number, ColumnType::Date, ColumnType::Number]);

        assert_eq!(table.rows[0], (2, r#"["Петрова А.С.",0.3,"2021-03-01",1000.5]"#.to_string()));
        assert_eq!(table.rows[20], (22, r#"[null,"н/д","—",null]"#.to_string()));

        let responsible = &table.columns[0];
        assert_eq!(responsible.top, [("Иванов".to_string(), 10), ("Петрова А.С.".to_string(), 10)]);
        assert_eq!(responsible.distinct, 2);
        assert_eq!((table.columns[1].min.as_deref(), table.columns[1].max.as_deref()), (Some("0.3"), Some("19000.3")));
        assert_eq!((table.columns[2].min.as_deref(), table.columns[2].max.as_deref()), (Some("2021-03-01"), Some("2021-03-20")));
        assert!(table.columns[1].top.is_empty());
    }
}
