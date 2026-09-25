//! Run the app's own question pipeline as a given user and print what would
//! go into the prompt — for "why wasn't X found?" and "what did it compute?"
//! without the UI.
//!
//!   APP_DATABASE_URL=… cargo run --example retrieve -- <username> "<question>" [top_k] [--answer] [--pick N] [--in <document id>]…
//!
//! Needs the app's servers running (chat on 8080, embeddings on 8081).
//! Calls `commands::query::prepare` — the function cmd_query uses — so the
//! path is the app's: embed, retrieve under RLS, and for spreadsheets the
//! planner and the table calculation (ADR-0022). When the app would ask the
//! user back, the options are printed; --pick N answers with option N, as
//! clicking its button does; repeat it for a chain of clarifications. --in limits the question to a document, as
//! choosing it in the chat panel does (repeatable). --answer also runs the completion. No LoRA
//! adapters: the example's client loads none.

use enclave_lib::{
    commands::query::{prepare, ChosenPlan, QueryArgs},
    llm::{CompletionRequest, LlmClient},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let answer = raw.iter().any(|a| a == "--answer");
    // One --pick per clarification, in order: a chosen option is checked
    // again and may ask the next question.
    let picks: Vec<usize> = raw
        .iter()
        .enumerate()
        .filter(|(i, _)| *i > 0 && raw[i - 1] == "--pick")
        .map(|(_, n)| n.parse())
        .collect::<Result<_, _>>()?;
    let scope: Vec<uuid::Uuid> = raw
        .iter()
        .enumerate()
        .filter(|(i, _)| *i > 0 && raw[i - 1] == "--in")
        .map(|(_, id)| id.parse())
        .collect::<Result<_, _>>()?;
    let takes_value = |i: usize| i > 0 && (raw[i - 1] == "--pick" || raw[i - 1] == "--in");
    let mut args = raw
        .iter()
        .enumerate()
        .filter(|(i, a)| !a.starts_with("--") && !takes_value(*i))
        .map(|(_, a)| a.clone());
    let usage = "usage: retrieve <username> \"<question>\" [top_k] [--answer] [--pick N]";
    let username = args.next().ok_or_else(|| anyhow::anyhow!(usage))?;
    let question = args.next().ok_or_else(|| anyhow::anyhow!(usage))?;
    let top_k: usize = args.next().map(|k| k.parse()).transpose()?.unwrap_or(5);

    let pool = sqlx::PgPool::connect(&std::env::var("APP_DATABASE_URL")?).await?;
    let user_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM users WHERE username = $1")
        .bind(&username)
        .fetch_one(&pool)
        .await?;

    let llm = LlmClient::connect("http://127.0.0.1:8080", "http://127.0.0.1:8081");
    let mut query = QueryArgs {
        query: question.clone(),
        top_k: Some(top_k),
        plan: None,
        document_ids: (!scope.is_empty()).then(|| scope.clone()),
    };
    let started = std::time::Instant::now();
    let mut prepared = prepare(&pool, &llm, user_id, &query).await.map_err(|e| anyhow::anyhow!(e))?;

    let mut picks = picks.into_iter();
    while let Some(c) = &prepared.clarification {
        println!("CLARIFICATION: {}", c.question);
        for (i, o) in c.options.iter().enumerate() {
            println!("  [{}] {}", i + 1, o.label);
        }
        let Some(n) = picks.next() else { return Ok(()) };
        let option = c.options.get(n.wrapping_sub(1)).ok_or_else(|| anyhow::anyhow!("no option {n}"))?;
        println!("picked [{n}] {}", option.label);
        query.plan = Some(ChosenPlan { table_id: c.table_id, plan: option.plan.clone() });
        prepared = prepare(&pool, &llm, user_id, &query).await.map_err(|e| anyhow::anyhow!(e))?;
    }
    println!("prepared in {:.0} ms", started.elapsed().as_secs_f64() * 1000.0);

    match &prepared.calculation {
        Some(calculation) => println!("CALCULATION\n{calculation}"),
        None => {
            println!("{} source(s) for {username}", prepared.sources.len());
            for (i, s) in prepared.sources.iter().enumerate() {
                println!("[Source {}] {}  rrf={:.4}\n    {}", i + 1, s.filename, s.score, s.excerpt.replace('\n', " ⏎ "));
            }
        }
    }

    if let (true, Some(ready)) = (answer, &prepared.answer) {
        println!("\nANSWER (written by the core):\n{ready}");
    } else if answer {
        let t = std::time::Instant::now();
        let done = llm
            .complete(&CompletionRequest {
                prompt: prepared.prompt,
                n_predict: 768,
                temperature: 0.3,
                ..Default::default()
            })
            .await?;
        println!("\nANSWER ({:.1} s):\n{}", t.elapsed().as_secs_f64(), done.trim());
    }
    Ok(())
}
