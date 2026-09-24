//! Run the app's own retrieval for a question, as a given user, and print
//! what would go into the prompt — for "why wasn't X found?" without the UI.
//!
//!   APP_DATABASE_URL=… cargo run --example retrieve -- <username> "<question>" [top_k] [--answer]
//!
//! Needs the embedding server (port 8081, started by the app); --answer also
//! asks the chat server (port 8080) with the app's own prompt
//! (commands::query::grounded_messages). Same path as
//! cmd_query: embed the question (search_query: prefix), then one
//! transaction with set_current_user → retrieval::retrieve, so RLS applies
//! exactly as in the app.

use enclave_lib::{commands::query::grounded_messages, db::rls::set_current_user, retrieval};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let answer = std::env::args().any(|a| a == "--answer");
    let mut args = std::env::args().skip(1).filter(|a| a != "--answer");
    let usage = "usage: retrieve <username> \"<question>\" [top_k] [--answer]";
    let username = args.next().ok_or_else(|| anyhow::anyhow!(usage))?;
    let question = args.next().ok_or_else(|| anyhow::anyhow!(usage))?;
    let top_k: usize = args.next().map(|k| k.parse()).transpose()?.unwrap_or(5);

    let started = std::time::Instant::now();
    let embedding: Vec<f32> = {
        #[derive(serde::Deserialize)]
        struct Resp { data: Vec<Item> }
        #[derive(serde::Deserialize)]
        struct Item { embedding: Vec<f32> }
        let resp: Resp = reqwest::Client::new()
            .post("http://127.0.0.1:8081/v1/embeddings")
            .json(&serde_json::json!({ "input": format!("search_query: {question}") }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        resp.data.into_iter().next().map(|d| d.embedding).ok_or_else(|| anyhow::anyhow!("no embedding"))?
    };
    let embedded = started.elapsed();

    let pool = sqlx::PgPool::connect(&std::env::var("APP_DATABASE_URL")?).await?;
    let user_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM users WHERE username = $1")
        .bind(&username)
        .fetch_one(&pool)
        .await?;

    let mut tx = pool.begin().await?;
    set_current_user(&mut tx, user_id).await?;
    let t = std::time::Instant::now();
    let chunks = retrieval::retrieve(&mut tx, &embedding, &question, top_k).await?;
    let retrieved = t.elapsed();
    tx.commit().await?;

    println!(
        "{} chunk(s) for {username}; embed {:.0} ms, retrieval {:.0} ms",
        chunks.len(),
        embedded.as_secs_f64() * 1000.0,
        retrieved.as_secs_f64() * 1000.0
    );
    for (i, c) in chunks.iter().enumerate() {
        let preview: String = c.content.chars().take(160).collect();
        println!("[Source {}] {}  rrf={:.4}\n    {}", i + 1, c.filename, c.score, preview.replace('\n', " ⏎ "));
    }

    if answer {
        let http = reqwest::Client::new();
        let (system, user) = grounded_messages(&chunks, &question);
        #[derive(serde::Deserialize)]
        struct Prompt { prompt: String }
        let prompt: Prompt = http
            .post("http://127.0.0.1:8080/apply-template")
            .json(&serde_json::json!({ "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ]}))
            .send().await?.error_for_status()?.json().await?;
        #[derive(serde::Deserialize)]
        struct Completion { content: String }
        let t = std::time::Instant::now();
        let done: Completion = http
            .post("http://127.0.0.1:8080/completion")
            .json(&serde_json::json!({ "prompt": prompt.prompt, "n_predict": 768, "temperature": 0.3 }))
            .send().await?.error_for_status()?.json().await?;
        println!("\nANSWER ({:.1} s):\n{}", t.elapsed().as_secs_f64(), done.content.trim());
    }
    Ok(())
}
