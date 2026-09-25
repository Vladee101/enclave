fn main() {
    // sqlx::migrate!("../migrations") embeds the migrations at compile time,
    // and Cargo does not know it reads that directory: without this a new
    // migration file is left out of the build until some Rust source changes.
    println!("cargo:rerun-if-changed=../migrations");
    tauri_build::build()
}
