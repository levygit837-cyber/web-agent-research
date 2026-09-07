use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

/// CLI protótipo do agente de pesquisa web (engine Obscura).
#[derive(Debug, Parser)]
#[command(name = "web-agent-research", version)]
struct Cli {
    /// Objetivo da Pesquisa a executar
    #[arg(default_value = "o que é Obscura headless browser?")]
    query: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    println!("pesquisa: {}", cli.query);
    println!("sessão: ainda não implementada (ver MISSION.md)");
    Ok(())
}
