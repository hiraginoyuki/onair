//! Local-only benchmark runner for synthetic LLM Protocol Alpha scenarios.

use std::path::PathBuf;

use clap::Parser;
use llm_protocol_onair_parity::{
    BenchmarkRunOptions, default_manifest_path, dry_run_report, prepare_benchmark_run,
    read_manifest, repository_root, run_live_benchmark, write_local_report,
};

#[derive(Debug, Parser)]
#[command(
    name = "llm-protocol-benchmark",
    about = "Run local-only synthetic LLM Protocol Alpha benchmark scenarios"
)]
struct Args {
    /// Path to the tracked synthetic scenario manifest.
    #[arg(long)]
    manifest: Option<PathBuf>,

    /// Permit a live provider benchmark after all additional safety checks.
    #[arg(long)]
    live: bool,

    /// Required alongside --live to confirm that provider requests may be sent.
    #[arg(long)]
    confirm_live: bool,

    /// Local-only JSON configuration with endpoints, credential header names,
    /// credential environment-variable names, and hard caps.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Local-only JSON output destination. Defaults below .local/ in live mode.
    #[arg(long)]
    output: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let repo_root = repository_root();
    let manifest_path = args
        .manifest
        .unwrap_or_else(|| default_manifest_path(&repo_root));
    let manifest = read_manifest(&manifest_path)?;
    let prepared = prepare_benchmark_run(
        &repo_root,
        &manifest,
        &BenchmarkRunOptions {
            live: args.live,
            confirmed: args.confirm_live,
            config_path: args.config,
            output_path: args.output,
        },
    )?;
    if !args.live {
        println!(
            "{}",
            serde_json::to_string_pretty(&dry_run_report(&manifest))?
        );
        return Ok(());
    }

    let report = run_live_benchmark(&manifest, &prepared).await?;
    let output = prepared
        .output_path
        .as_deref()
        .expect("live benchmark preparation supplies a local output path");
    write_local_report(&repo_root, output, &report)?;
    println!(
        "Live benchmark completed. Redacted observations were written to {}.",
        output.display()
    );
    Ok(())
}
