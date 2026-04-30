// Dumps the prefetched context that prompts.rs would embed for a given patch,
// so we can compare it offline against the tool calls the model actually made.

use anyhow::{Context, Result};
use clap::Parser;
use sashiko::worker::prefetch::prefetch_context;
use std::path::PathBuf;

#[derive(Parser, Debug)]
struct Args {
    /// Path to the worktree root (the linux kernel checkout).
    #[arg(long)]
    worktree: PathBuf,

    /// Path to the patch file (mbox or raw diff).
    #[arg(long)]
    patch: PathBuf,
}

fn extract_diff(raw: &str) -> &str {
    match raw.find("\ndiff --git ") {
        Some(idx) => &raw[idx + 1..],
        None => raw,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let raw = std::fs::read_to_string(&args.patch)
        .with_context(|| format!("reading {}", args.patch.display()))?;
    let diff = extract_diff(&raw);
    let prefetched = prefetch_context(&args.worktree, diff).await?;
    print!("{}", prefetched);
    Ok(())
}
