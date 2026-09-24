//! Calculations over spreadsheet tables (ADR-0022), on a real database:
//! the app's own `tables` code writes a sheet the way the worker does, then
//! plans run as a member under RLS — exact sums, counts, groups, date
//! prefixes — and as a non-member, who sees no table and no rows. Deleting
//! the document removes its tables.
//!
//! The planner (a model call) is not exercised here; plans are given as
//! the JSON the model would produce and go through the same `parse_plan`.
//!
//! Same environment as rls_validation (TEST_ADMIN_URL / TEST_APP_URL, skip
//! unless ENCLAVE_REQUIRE_DB_TESTS), its own throwaway database.

use enclave_lib::{
    db::rls::set_current_user,
    ingest::extract::{CellValue, Sheet, SheetRow},
    tables::{self, plan},
};
use sqlx::PgPool;
use uuid::Uuid;

const TEST_DB_NAME: &str = "enclave_tables_test";

fn with_database(url: &str, db_name: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url, None),
    };
    let last_slash = base.rfind('/').expect("connection URL must contain a path");
    let mut new_url = format!("{}/{}", &base[..last_slash], db_name);
    if let Some(q) = query {
        new_url.push('?');
        new_url.push_str(q);
    }
    new_url
}

/// 300 contracts: three managers in turn, amounts n × 1000.5, signed one a
/// day from 2020-12-01 (so 2021 starts at row 32), plus one row whose
/// amount is "н/д" — a number column with a stray text cell.
fn registry() -> Sheet {
    use CellValue::*;
    let managers = ["Петрова А.С.", "Иванов И.И.", "Сидоров П.П."];
    let start = chrono::NaiveDate::from_ymd_opt(2020, 12, 1).unwrap();
    let mut rows: Vec<SheetRow> = (1..=300)
        .map(|n| SheetRow {
            number: n + 1,
            cells:  vec![
                Some(Text(format!("Д-{n:06}"))),
                Some(Number(n as f64 * 1000.5)),
                Some(Date((start + chrono::Days::new(n as u64 - 1)).format("%Y-%m-%d").to_string())),
                Some(Text(managers[n % 3].to_string())),
            ],
        })
        .collect();
    rows.push(SheetRow { number: 302, cells: vec![Some(Text("Д-999999".into())), Some(Text("н/д".into())), None, Some(Text("Петрова А.С.".into()))] });
    Sheet {
        name:    "Реестр".into(),
        columns: vec!["Номер договора".into(), "Сумма, руб.".into(), "Дата подписания".into(), "Ответственный".into()],
        rows,
    }
}

async fn run(pool: &PgPool, user: Uuid, doc: Uuid, plan_json: &str) -> Result<Option<plan::Computation>, Box<dyn std::error::Error>> {
    let mut tx = pool.begin().await?;
    set_current_user(&mut tx, user).await?;
    let candidates = plan::load_candidates(&mut tx, &[doc]).await?;
    if candidates.is_empty() {
        tx.rollback().await?;
        return Ok(None);
    }
    let plan = plan::parse_plan(plan_json, &candidates)?.expect("answerable plan");
    let computation = plan::execute(&mut tx, &candidates, plan).await?;
    tx.rollback().await?;
    Ok(Some(computation))
}

fn plan_json(filters: &str, group_by: &str, metric: &str) -> String {
    format!(r#"{{"answerable": true, "table": "T1", "filters": [{filters}], "group_by": {group_by}, "metric": {metric}, "order": "desc"}}"#)
}

#[tokio::test]
async fn calculations_are_exact_and_isolated() -> Result<(), Box<dyn std::error::Error>> {
    let (Ok(admin_url), Ok(app_url)) = (std::env::var("TEST_ADMIN_URL"), std::env::var("TEST_APP_URL")) else {
        if std::env::var_os("ENCLAVE_REQUIRE_DB_TESTS").is_some() {
            panic!("ENCLAVE_REQUIRE_DB_TESTS is set but TEST_ADMIN_URL / TEST_APP_URL are not");
        }
        eprintln!("TEST_ADMIN_URL / TEST_APP_URL not set — skipping table_aggregation.");
        return Ok(());
    };

    let root = PgPool::connect(&with_database(&admin_url, "postgres")).await?;
    sqlx::query(&format!("DROP DATABASE IF EXISTS {TEST_DB_NAME}")).execute(&root).await?;
    sqlx::query(&format!("CREATE DATABASE {TEST_DB_NAME}")).execute(&root).await?;
    root.close().await;

    let admin = PgPool::connect(&with_database(&admin_url, TEST_DB_NAME)).await?;
    sqlx::migrate!("../migrations").run(&admin).await?;

    let dept: Uuid = sqlx::query_scalar("INSERT INTO departments (name, slug) VALUES ('Договоры', 'contracts') RETURNING id")
        .fetch_one(&admin)
        .await?;
    let other_dept: Uuid = sqlx::query_scalar("INSERT INTO departments (name, slug) VALUES ('Другой', 'other') RETURNING id")
        .fetch_one(&admin)
        .await?;
    let member: Uuid = sqlx::query_scalar("INSERT INTO users (username, email, password_hash) VALUES ('member', 'm@local', 'x') RETURNING id")
        .fetch_one(&admin)
        .await?;
    let outsider: Uuid = sqlx::query_scalar("INSERT INTO users (username, email, password_hash) VALUES ('outsider', 'o@local', 'x') RETURNING id")
        .fetch_one(&admin)
        .await?;
    sqlx::query("INSERT INTO department_members VALUES ($1, $2), ($3, $4)")
        .bind(member)
        .bind(dept)
        .bind(outsider)
        .bind(other_dept)
        .execute(&admin)
        .await?;
    let doc: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (department_id, title, file_hash, mime_type, byte_size, status, uploaded_by) \
         VALUES ($1, 'Реестр.xlsx', 'h', 'application/vnd.ms-excel', 1, 'ready', $2) RETURNING id",
    )
    .bind(dept)
    .bind(member)
    .fetch_one(&admin)
    .await?;

    // Written as the worker writes it (the admin pool stands in for
    // ingest_pool: both bypass RLS).
    let table = tables::build_table(&registry());
    let mut tx = admin.begin().await?;
    tables::write_tables(&mut tx, doc, dept, &[table]).await?;
    tx.commit().await?;

    let app = PgPool::connect(&with_database(&app_url, TEST_DB_NAME)).await?;
    let sum = r#"{"fn": "sum", "column": "Сумма, руб."}"#;
    let count = r#"{"fn": "count", "column": null}"#;

    // Everything: 1000.5 × (1 + … + 300) = 45 172 575 — exact decimal
    // arithmetic, and the "н/д" row is counted as a row but not summed.
    let all = run(&app, member, doc, &plan_json("", "null", sum)).await?.expect("member sees the table");
    assert_eq!(all.matched_rows, 301);
    assert_eq!(all.groups[0].value.as_deref(), Some("45172575.0"));

    // One manager (n % 3 == 0: n = 3, 6, …, 300 → 100 rows) plus the н/д
    // row, which is hers too; "петрова" matches case-insensitively.
    let hers = run(&app, member, doc, &plan_json(r#"{"column": "Ответственный", "op": "contains", "value": "петрова"}"#, "null", count))
        .await?
        .unwrap();
    assert_eq!(hers.groups[0].value.as_deref(), Some("101"));

    // A year: rows signed in 2021 are n = 32 … 300 (the table ends in
    // September 2021), 269 rows; "> 2020" means the same.
    for filter in [
        r#"{"column": "Дата подписания", "op": "=", "value": "2021"}"#,
        r#"{"column": "Дата подписания", "op": ">", "value": "2020"}"#,
    ] {
        let year = run(&app, member, doc, &plan_json(filter, "null", count)).await?.unwrap();
        assert_eq!(year.groups[0].value.as_deref(), Some("269"), "{filter}");
    }

    // Numbers compare as numbers: n × 1000.5 >= 100 000 from n = 100.
    let big = run(&app, member, doc, &plan_json(r#"{"column": "Сумма, руб.", "op": ">=", "value": "100 000"}"#, "null", count))
        .await?
        .unwrap();
    assert_eq!(big.groups[0].value.as_deref(), Some("201"));

    // Grouped, largest first: 1000.5 × Σn over each manager's rows —
    // n ≡ 0: 15 150, n ≡ 2: 15 050, n ≡ 1: 14 950.
    let per_manager = run(&app, member, doc, &plan_json("", r#"{"column": "Ответственный"}"#, sum)).await?.unwrap();
    let got: Vec<(Option<&str>, Option<&str>)> =
        per_manager.groups.iter().map(|g| (g.key.as_deref(), g.value.as_deref())).collect();
    assert_eq!(
        got,
        [
            (Some("Петрова А.С."), Some("15157575.0")),
            (Some("Сидоров П.П."), Some("15057525.0")),
            (Some("Иванов И.И."), Some("14957475.0")),
        ]
    );
    assert_eq!((per_manager.matched_rows, per_manager.total_groups), (301, 3));

    // By year: 2020 has 31 rows, 2021 has 269; the н/д row has no date.
    let per_year = run(&app, member, doc, &plan_json("", r#"{"column": "Дата подписания", "period": "year"}"#, count))
        .await?
        .unwrap();
    let got: Vec<(Option<&str>, i64)> = per_year.groups.iter().map(|g| (g.key.as_deref(), g.rows)).collect();
    assert_eq!(got, [(Some("2021"), 269), (Some("2020"), 31), (None, 1)]);

    // The description names the file, the conditions and the result.
    let text = hers.describe();
    assert!(text.contains("«Реестр.xlsx»") && text.contains("содержит «петрова»") && text.contains("Подошло строк: 101"), "{text}");

    // Another department: no table to plan over — and even with the
    // member's plan in hand, RLS leaves no rows to compute from.
    assert!(run(&app, outsider, doc, &plan_json("", "null", sum)).await?.is_none(), "an outsider must see no table");
    {
        let mut tx = app.begin().await?;
        set_current_user(&mut tx, member).await?;
        let candidates = plan::load_candidates(&mut tx, &[doc]).await?;
        tx.rollback().await?;
        let mut tx = app.begin().await?;
        set_current_user(&mut tx, outsider).await?;
        let plan = plan::parse_plan(&plan_json("", "null", count), &candidates)?.unwrap();
        let foreign = plan::execute(&mut tx, &candidates, plan).await?;
        tx.rollback().await?;
        assert_eq!(foreign.matched_rows, 0, "RLS must hide the rows from an outsider");
        assert_eq!(foreign.groups[0].value.as_deref(), Some("0"));
    }

    // Deleting the document takes its tables with it (trigger, ADR-0022).
    let mut tx = app.begin().await?;
    set_current_user(&mut tx, member).await?;
    sqlx::query("SELECT * FROM delete_document($1)").bind(doc).execute(&mut *tx).await?;
    tx.commit().await?;
    let left: (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM sheet_tables), (SELECT count(*) FROM sheet_rows)")
        .fetch_one(&admin)
        .await?;
    assert_eq!(left, (0, 0), "a deleted document's tables must be purged");

    Ok(())
}
