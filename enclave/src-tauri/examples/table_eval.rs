//! Check the table questions end to end against numbers from SQL
//! (ADR-0022/0023/0024): each question goes through the app's own path —
//! `commands::query::prepare`, the planner on the live model, the
//! clarifications, the compiled aggregate under RLS — and the result is
//! compared with hand-written SQL over the same rows, run as the admin.
//! The reference SQL shares nothing with the planner or the compiler, so a
//! wrong plan, a wrong filter or a wrong compilation all show up.
//!
//!   ADMIN_DATABASE_URL=… APP_DATABASE_URL=… \
//!     cargo run --example table_eval -- [cases.json] [--only N]…
//!
//! Needs the app's model servers running (chat on 8080, embeddings on 8081)
//! and the registry from the cases file uploaded and ingested. The cases
//! default to `examples/table_eval.json`. Exit code 1 if any case fails.
//!
//! A case's `picks` are the buttons a user who knows the answer would
//! press: at each clarification the first pick matching an option's label
//! is taken. The model may ask in a different order or not at all — both
//! pass, as long as the number is right; the report shows what was asked.

use anyhow::{bail, Context, Result};
use enclave_lib::{
    commands::query::{prepare, ChosenPlan, Prepared, QueryArgs},
    llm::LlmClient,
    tables::plan::Computation,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Instant;
use uuid::Uuid;

#[derive(Deserialize)]
struct Suite {
    user:  String,
    table: TableRef,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct TableRef {
    document: String,
    sheet:    String,
}

#[derive(Deserialize)]
struct Case {
    question: String,
    #[serde(default)]
    picks:    Vec<String>,
    expect:   Expect,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Expect {
    /// One value, or (group, value) rows, from SQL over `rows`.
    Sql(String),
    /// Answered by search, not a calculation, with this text in a source.
    Retrieval(String),
    /// Either way is right, as long as the answer rests on this text: a
    /// source excerpt, or a group of the calculation. For questions a table
    /// can answer by listing values («what is the subject of the contracts»)
    /// as well as the text can.
    #[serde(rename = "answer_contains")]
    AnswerContains(String),
}

/// Numbers compare as numbers ("31188518750.0" = "31188518750",
/// "2495040.7500" = "2495040.75"); anything else as text.
fn same(a: &str, b: &str) -> bool {
    match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
        (Ok(x), Ok(y)) => (x - y).abs() < 0.005,
        _ => a.trim() == b.trim(),
    }
}

fn one_line(s: &str) -> String {
    s.replace('\n', " ")
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "∅".into(),
        other => other.to_string(),
    }
}

/// The reference: `(group, value)` pairs; group is empty for one value.
async fn reference(admin: &sqlx::PgPool, table_id: Uuid, sql: &str) -> Result<Vec<(String, String)>> {
    let wrapped = format!(
        "WITH rows AS (SELECT cells FROM sheet_rows WHERE table_id = $1) \
         SELECT row_to_json(q)::text FROM ({sql}) q"
    );
    let rows: Vec<String> = sqlx::query_scalar(&wrapped)
        .bind(table_id)
        .fetch_all(admin)
        .await
        .with_context(|| format!("reference SQL failed: {sql}"))?;
    rows.iter()
        .map(|r| {
            // row_to_json keeps the column order (serde_json preserve_order).
            let obj: serde_json::Map<String, Value> = serde_json::from_str(r)?;
            let cols: Vec<&Value> = obj.values().collect();
            Ok(match cols.as_slice() {
                [v] => (String::new(), text(v)),
                [k, v] => (text(k), text(v)),
                _ => bail!("reference SQL must return one or two columns: {sql}"),
            })
        })
        .collect()
}

/// What the app computed, in the same shape.
fn computed(c: &Computation) -> Vec<(String, String)> {
    c.groups
        .iter()
        .map(|g| (g.key.clone().unwrap_or_default(), g.value.clone().unwrap_or_else(|| "∅".into())))
        .collect()
}

fn compare(got: &Computation, want: &[(String, String)]) -> std::result::Result<String, String> {
    let have = computed(got);
    if want.len() == 1 && want[0].0.is_empty() {
        let (w, h) = (&want[0].1, have.first().map(|(_, v)| v.as_str()).unwrap_or("∅"));
        return if have.len() == 1 && same(h, w) { Ok(h.to_string()) } else { Err(format!("got {h}, expected {w}")) };
    }
    if got.total_groups as usize != want.len() {
        return Err(format!("{} groups, expected {}", got.total_groups, want.len()));
    }
    let want: BTreeMap<&str, &str> = want.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut wrong = Vec::new();
    for (k, v) in &have {
        match want.get(k.as_str()) {
            Some(w) if same(v, w) => {}
            Some(w) => wrong.push(format!("{k}: {v} ≠ {w}")),
            None => wrong.push(format!("{k}: unexpected group")),
        }
    }
    if wrong.is_empty() {
        Ok(have.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", "))
    } else {
        Err(wrong.join("; "))
    }
}

struct Outcome {
    passed: bool,
    detail: String,
    asked:  Vec<String>,
    ms:     u128,
}

async fn run_case(
    admin: &sqlx::PgPool,
    app: &sqlx::PgPool,
    llm: &LlmClient,
    user_id: Uuid,
    table_id: Uuid,
    case: &Case,
) -> Result<Outcome> {
    let started = Instant::now();
    let mut args = QueryArgs { query: case.question.clone(), top_k: Some(5), plan: None, document_ids: None };
    let mut picks = case.picks.clone();
    let mut asked = Vec::new();
    let mut prepared: Prepared = prepare(app, llm, user_id, &args).await.map_err(|e| anyhow::anyhow!(e))?;

    while let Some(c) = &prepared.clarification {
        let labels: Vec<&str> = c.options.iter().map(|o| o.label.as_str()).collect();
        let choice = picks
            .iter()
            .position(|p| labels.iter().any(|l| l.contains(p.as_str())))
            .map(|i| picks.remove(i));
        let Some(pick) = choice else {
            return Ok(Outcome {
                passed: false,
                detail: format!("asked «{}» [{}], no pick matches", c.question, labels.join(" | ")),
                asked,
                ms: started.elapsed().as_millis(),
            });
        };
        let option = c.options.iter().find(|o| o.label.contains(pick.as_str())).expect("matched above");
        asked.push(format!("{} → {}", c.question, option.label));
        args.plan = Some(ChosenPlan { table_id: c.table_id, plan: option.plan.clone() });
        prepared = prepare(app, llm, user_id, &args).await.map_err(|e| anyhow::anyhow!(e))?;
    }

    let in_sources = |needle: &str| {
        if prepared.sources.iter().any(|s| s.excerpt.contains(needle)) {
            Ok(format!("search, «{needle}» in the sources"))
        } else {
            Err(format!("search, but no source contains «{needle}»"))
        }
    };
    let result = match (&case.expect, &prepared.computation) {
        (Expect::Sql(sql), Some(c)) => {
            if c.candidate.table_id != table_id {
                Err(format!("calculated over another table ({})", c.candidate.filename))
            } else {
                compare(c, &reference(admin, table_id, sql).await?)
            }
        }
        (Expect::Sql(_), None) => Err("answered by search, expected a calculation".into()),
        (Expect::Retrieval(_), Some(c)) => Err(format!("calculated ({}), expected search", one_line(&c.describe()))),
        (Expect::Retrieval(needle), None) => in_sources(needle),
        (Expect::AnswerContains(needle), Some(c)) => {
            let groups = computed(c);
            if groups.iter().any(|(k, _)| k.contains(needle.as_str())) {
                Ok(format!(
                    "calculation, «{needle}» among the groups: {}",
                    groups.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", ")
                ))
            } else {
                Err(format!("calculated, but no group contains «{needle}»: {}", one_line(&c.describe())))
            }
        }
        (Expect::AnswerContains(needle), None) => in_sources(needle),
    };
    let (passed, detail) = match result {
        Ok(d) => (true, d),
        Err(d) => (false, d),
    };
    Ok(Outcome { passed, detail, asked, ms: started.elapsed().as_millis() })
}

#[tokio::main]
async fn main() -> Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let only: Vec<usize> = raw
        .iter()
        .enumerate()
        .filter(|(i, _)| *i > 0 && raw[i - 1] == "--only")
        .map(|(_, n)| n.parse())
        .collect::<Result<_, _>>()?;
    let path = raw
        .iter()
        .enumerate()
        .find(|(i, a)| !a.starts_with("--") && !(*i > 0 && raw[i - 1] == "--only"))
        .map(|(_, a)| a.clone())
        .unwrap_or_else(|| concat!(env!("CARGO_MANIFEST_DIR"), "/examples/table_eval.json").to_string());
    let suite: Suite = serde_json::from_str(&std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?)
        .with_context(|| format!("parsing {path}"))?;

    let admin = sqlx::PgPool::connect(&std::env::var("ADMIN_DATABASE_URL").context("ADMIN_DATABASE_URL")?).await?;
    let app = sqlx::PgPool::connect(&std::env::var("APP_DATABASE_URL").context("APP_DATABASE_URL")?).await?;
    let user_id: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE username = $1")
        .bind(&suite.user)
        .fetch_one(&admin)
        .await
        .with_context(|| format!("no user {}", suite.user))?;
    let table_id: Uuid = sqlx::query_scalar(
        "SELECT t.id FROM sheet_tables t JOIN documents d ON d.id = t.document_id \
         WHERE d.title = $1 AND t.sheet = $2 AND d.deleted_at IS NULL",
    )
    .bind(&suite.table.document)
    .bind(&suite.table.sheet)
    .fetch_one(&admin)
    .await
    .with_context(|| format!("no live table «{}» / «{}»", suite.table.document, suite.table.sheet))?;
    let llm = LlmClient::connect("http://127.0.0.1:8080", "http://127.0.0.1:8081");

    let (mut passed, mut total) = (0, 0);
    for (i, case) in suite.cases.iter().enumerate() {
        let n = i + 1;
        if !only.is_empty() && !only.contains(&n) {
            continue;
        }
        total += 1;
        let outcome = run_case(&admin, &app, &llm, user_id, table_id, case).await?;
        passed += outcome.passed as usize;
        println!(
            "{} {n:>2}. {}  ({:.1} s)\n      {}",
            if outcome.passed { "✓" } else { "✗" },
            case.question,
            outcome.ms as f64 / 1000.0,
            outcome.detail
        );
        for a in &outcome.asked {
            println!("      asked: {a}");
        }
    }
    println!("\n{passed} / {total} correct");
    if passed < total {
        std::process::exit(1);
    }
    Ok(())
}
