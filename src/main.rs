use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;
use web_agent_research::{render, run_research, ResearchError, ResearchRequest};
#[derive(Debug, Parser)]
#[command(name = "web-agent-research", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Research goal to execute (legacy positional form without subcommand).
    goal: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run one research session to final synthesis.
    Research {
        /// Research goal to execute.
        goal: String,
        /// Synthesis size (`deep` reserves vocabulary; rejected at parse).
        #[arg(long, value_enum, default_value_t = SizeArg::Medium)]
        size: SizeArg,
        /// Total gateway calls allowed.
        #[arg(long, default_value_t = 8)]
        max_turns: u32,
        /// Session id; defaults to `<unix-secs>-<pid>`.
        #[arg(long)]
        session_id: Option<String>,
        /// Session file; defaults to `sessions/<id>.jsonl`.
        #[arg(long)]
        session_out: Option<PathBuf>,
        /// Print the `ResearchResponse` verbatim as JSON.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SizeArg {
    Small,
    Medium,
    Large,
}

impl SizeArg {
    fn as_size(self) -> web_agent_research::shared::types::synthesis::SynthesisSize {
        use web_agent_research::shared::types::synthesis::SynthesisSize;
        match self {
            Self::Small => SynthesisSize::Small,
            Self::Medium => SynthesisSize::Medium,
            Self::Large => SynthesisSize::Large,
        }
    }
}

fn exit_for(err: &ResearchError) -> i32 {
    match err {
        ResearchError::NotConfigured(_) => 2,
        ResearchError::GatewayExhausted(_) => 3,
        ResearchError::ToolFailure(_) => 4,
        ResearchError::BudgetExhausted { .. } => 5,
        ResearchError::Io(_) => 6,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let research = match cli.command {
        Some(Command::Research {
            goal,
            size,
            max_turns,
            session_id,
            session_out,
            json,
        }) => (goal, size, max_turns, session_id, session_out, json),
        None => (
            cli.goal
                .unwrap_or_else(|| "what is the Obscura headless browser?".to_owned()),
            SizeArg::Medium,
            8,
            None,
            None,
            false,
        ),
    };
    let (goal, size, max_turns, session_id, session_out, json) = research;
    let req = ResearchRequest {
        goal,
        size: size.as_size(),
        max_turns,
        session_id,
        session_out,
    };
    match run_research(req).await {
        Ok(response) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&response).expect("response serializes")
                );
            } else {
                print!("{}", render(&response));
            }
            Ok(())
        }
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(exit_for(&err));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use web_agent_research::{CitationDTO, ResearchResponse, SynthesisDTO, ThemeDTO, UsageDTO};

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("args must parse")
    }

    #[test]
    fn research_subcommand_maps_to_request_fields() {
        let cli = parse(&[
            "web-agent-research",
            "research",
            "what is the Obscura headless browser?",
            "--size",
            "small",
            "--max-turns",
            "4",
            "--session-id",
            "s-1",
            "--json",
        ]);
        let Some(Command::Research {
            goal,
            size,
            max_turns,
            session_id,
            json,
            ..
        }) = cli.command
        else {
            panic!("expected research subcommand");
        };
        assert_eq!(goal, "what is the Obscura headless browser?");
        assert_eq!(size, SizeArg::Small);
        assert_eq!(max_turns, 4);
        assert_eq!(session_id.as_deref(), Some("s-1"));
        assert!(json);
    }

    #[test]
    fn deep_size_rejected_at_parse() {
        let err = Cli::try_parse_from(["web-agent-research", "research", "goal", "--size", "deep"])
            .expect_err("deep must not parse");
        let message = err.to_string();
        assert!(
            message.contains("deep"),
            "parse error must name the rejected value, got {message}"
        );
    }

    #[test]
    fn error_mapping_covers_every_variant() {
        assert_eq!(exit_for(&ResearchError::NotConfigured("x".to_owned())), 2);
        assert_eq!(
            exit_for(&ResearchError::GatewayExhausted("x".to_owned())),
            3
        );
        assert_eq!(exit_for(&ResearchError::ToolFailure("x".to_owned())), 4);
        assert_eq!(exit_for(&ResearchError::BudgetExhausted { turns: 8 }), 5);
        assert_eq!(exit_for(&ResearchError::Io("x".to_owned())), 6);
    }

    #[test]
    fn render_goes_to_stdout_shape() {
        let response = ResearchResponse {
            session_id: "s".to_owned(),
            synthesis: SynthesisDTO {
                size: web_agent_research::shared::types::synthesis::SynthesisSize::Small,
                summary: "summary text".to_owned(),
                themes: vec![ThemeDTO {
                    title: "Findings".to_owned(),
                    points: vec!["point one".to_owned()],
                    citations: vec![],
                }],
                citations: vec![CitationDTO {
                    url: "https://example.com/t".to_owned(),
                    title: Some("t".to_owned()),
                }],
            },
            turns_used: 1,
            evidence_urls: vec!["https://example.com/t".to_owned()],
            usage: UsageDTO {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                reasoning_tokens: 0,
            },
        };
        let printed = render(&response);
        assert!(printed.contains("summary text"));
        assert!(printed.contains("Sources:"));
        assert!(printed.contains("https://example.com/t"));
    }
}
