//! Run the app's own question pipeline as a given user and print what would
//! go into the prompt — for "why wasn't X found?" and "what did it compute?"
//! without the UI.
//!
//!   APP_DATABASE_URL=… cargo run --example retrieve -- <username> "<question>" [top_k] [--answer]
//!
//! Needs the app's servers running (chat on 8080, embeddings on 8081).
//! Calls `commands::query::prepare` — the function cmd_query uses — so the
//! path is the app's: embed, retrieve under RLS, and for spreadsheets the
//! planner and the table calculation (ADR-0022). --answer also runs the
//! completion. No LoRA adapters: the example's client loads none.

use enclave_lib::{
    commands::query::{prepare, QueryArgs},
    llm::{CompletionRequest, LlmClient},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let answer = std::env::args().any(|a| a == "--answer");
    let mut args = std::env::args().skip(1).filter(|a| a != "--answer");
    let usage = "usage: retrieve <username> \"<question>\" [top_k] [--answer]";
    let username = args.next().ok_or_else(|| anyhow::anyhow!(usage))?;
    let question = args.next().ok_or_else(|| anyhow::anyhow!(usage))?;
    let top_k: usize = args.next().map(|k| k.parse()).transpose()?.unwrap_or(5);

    let pool = sqlx::PgPool::connect(&std::env::var("APP_DATABASE_URL")?).await?;
    let user_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM users WHERE username = $1")
        .bind(&username)
        .fetch_one(&pool)
        .await?;

    let llm = LlmClient::connect("http://127.0.0.1:8080", "http://127.0.0.1:8081");
    let started = std::time::Instant::now();
    let prepared = prepare(&pool, &llm, user_id, &QueryArgs { query: question.clone(), top_k: Some(top_k) })
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
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

    if answer {
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
