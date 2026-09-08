use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

/// Prototype CLI for the web research agent (Obscura engine).
#[derive(Debug, Parser)]
#[command(name = "web-agent-research", version)]
struct Cli {
    /// Research goal to execute
    #[arg(default_value = "what is the Obscura headless browser?")]
    query: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    println!("research: {}", cli.query);
    println!("session: not implemented yet");
    Ok(())
}
