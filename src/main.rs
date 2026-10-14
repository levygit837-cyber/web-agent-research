use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;
use web_agent_research::{
    render, run_research, EventsDetail, EventsSink, ProgressMode, ResearchRequest, CLEAR_LINE,
};
#[derive(Debug, Parser)]
#[command(name = "web-agent-research", version, arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run one research session to final synthesis. `ctrl_c` ends it with exit 130.
    Research {
        /// Research goal to execute.
        goal: String,
        /// Synthesis size (`deep` reserves vocabulary; rejected at parse).
        #[arg(long, value_enum, default_value_t = SizeArg::Medium)]
        size: SizeArg,
        /// Base turn budget (gateway calls); each successful search after the first adds 5, up to `--max-turns-cap`.
        #[arg(long, default_value_t = 10)]
        max_turns: u32,
        /// Ceiling the turn budget can grow to; must be >= `--max-turns`.
        #[arg(long, default_value_t = 25)]
        max_turns_cap: u32,
        /// Wall-clock bound on the whole run, in seconds; must be >= 1.
        #[arg(long, default_value_t = 300)]
        deadline_secs: u64,
        /// Session id, `[A-Za-z0-9._-]{1,64}` without a leading dot; defaults to `<unix-secs>-<pid>`.
        #[arg(long)]
        session_id: Option<String>,
        /// Session file; defaults to `<data root>/sessions/<id>.jsonl` (best-effort). An explicit path must be writable.
        #[arg(long)]
        session_out: Option<PathBuf>,
        /// Write the Session's lifecycle Events as NDJSON, one per line, to a file (created 0600) or `-` for stderr.
        #[arg(long, value_name = "PATH|-")]
        events: Option<PathBuf>,
        /// How much of the model stream `--events` carries: `normal` leaves out `model.delta`, `tokens` adds every streamed fragment (high volume).
        #[arg(long, value_enum, default_value_t = EventsDetailArg::Normal)]
        events_detail: EventsDetailArg,
        /// Also record every model exchange (messages sent, reply, tool results) in the Session file. Off by default: rows hold page text and the goal.
        #[arg(long, default_value_t = false)]
        trace: bool,
        /// Human progress on stderr: `auto` (lines and a live status on a terminal, nothing when piped), `plain` (lines, no ANSI), `off`. `--events -` turns it off.
        #[arg(long, value_enum, default_value_t = ProgressArg::Auto)]
        progress: ProgressArg,
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
    fn as_size(self) -> web_agent_research::research::synthesis::SynthesisSize {
        use web_agent_research::research::synthesis::SynthesisSize;
        match self {
            Self::Small => SynthesisSize::Small,
            Self::Medium => SynthesisSize::Medium,
            Self::Large => SynthesisSize::Large,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum EventsDetailArg {
    Normal,
    Tokens,
}

impl EventsDetailArg {
    fn as_detail(self) -> EventsDetail {
        match self {
            Self::Normal => EventsDetail::Normal,
            Self::Tokens => EventsDetail::Tokens,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ProgressArg {
    Auto,
    Plain,
    Off,
}

impl ProgressArg {
    fn as_mode(self) -> ProgressMode {
        match self {
            Self::Auto => ProgressMode::Auto,
            Self::Plain => ProgressMode::Plain,
            Self::Off => ProgressMode::Off,
        }
    }
}

/// Stderr for `tracing` lines while Progress keeps a transient status line
/// there: each event is written after an erase-line, so the log line starts
/// on a clean row. Progress redraws its status on its next Event.
struct StatusAwareStderr;

impl Write for StatusAwareStderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut stderr = std::io::stderr().lock();
        stderr.write_all(CLEAR_LINE.as_bytes())?;
        stderr.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

/// `--events -` is stderr; any other value is a file path.
fn events_sink(arg: Option<PathBuf>) -> Option<EventsSink> {
    arg.map(|path| {
        if path.as_os_str() == "-" {
            EventsSink::Stderr
        } else {
            EventsSink::File(path)
        }
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    // stdout carries only the response (`--json` must parse as one JSON
    // object); logs go to stderr, with ANSI colour only on a terminal.
    let cli = Cli::parse();
    let stderr_is_tty = std::io::stderr().is_terminal();
    let status_line = match &cli.command {
        Command::Research {
            progress, events, ..
        } => progress
            .as_mode()
            .draws_status_line(events_sink(events.clone()).as_ref(), stderr_is_tty),
    };
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_ansi(stderr_is_tty);
    if status_line {
        subscriber.with_writer(|| StatusAwareStderr).init();
    } else {
        subscriber.with_writer(std::io::stderr).init();
    }

    let Command::Research {
        goal,
        size,
        max_turns,
        max_turns_cap,
        deadline_secs,
        session_id,
        session_out,
        events,
        events_detail,
        trace,
        progress,
        json,
    } = cli.command;
    let req = ResearchRequest {
        goal,
        size: size.as_size(),
        max_turns,
        max_turns_cap,
        deadline_secs,
        session_id,
        session_out,
        events: events_sink(events),
        events_detail: events_detail.as_detail(),
        trace,
        progress: progress.as_mode(),
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
            std::process::exit(err.exit_code());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use web_agent_research::research::synthesis::Support;
    use web_agent_research::{
        CitationDTO, ResearchResponse, SynthesisDTO, ThemeDTO, UsageDTO, VerificationDTO,
        CONTENT_TRUST_UNTRUSTED_WEB,
    };

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
        let Command::Research {
            goal,
            size,
            max_turns,
            session_id,
            json,
            ..
        } = cli.command;
        assert_eq!(goal, "what is the Obscura headless browser?");
        assert_eq!(size, SizeArg::Small);
        assert_eq!(max_turns, 4);
        assert_eq!(session_id.as_deref(), Some("s-1"));
        assert!(json);
    }

    /// The CLI defaults are the request defaults a JSON caller gets.
    #[test]
    fn cli_defaults_match_request_defaults() {
        let Command::Research {
            max_turns,
            max_turns_cap,
            deadline_secs,
            ..
        } = parse(&["web-agent-research", "research", "goal"]).command;
        let req: ResearchRequest =
            serde_json::from_str(r#"{"goal":"goal"}"#).expect("request deserializes");
        assert_eq!((max_turns, max_turns_cap, deadline_secs), (10, 25, 300));
        assert_eq!(
            (req.max_turns, req.max_turns_cap, req.deadline_secs),
            (max_turns, max_turns_cap, deadline_secs)
        );
    }

    #[test]
    fn budget_and_deadline_flags_parse() {
        let Command::Research {
            max_turns_cap,
            deadline_secs,
            ..
        } = parse(&[
            "web-agent-research",
            "research",
            "goal",
            "--max-turns-cap",
            "12",
            "--deadline-secs",
            "30",
        ])
        .command;
        assert_eq!(max_turns_cap, 12);
        assert_eq!(deadline_secs, 30);
    }

    #[test]
    fn bare_invocation_prints_help_without_research() {
        let err = Cli::try_parse_from(["web-agent-research"])
            .expect_err("bare invocation must not parse into a research");
        assert!(
            err.to_string().to_lowercase().contains("usage"),
            "bare invocation must print help/usage, got {err}",
        );
    }

    #[test]
    fn bare_positional_goal_without_subcommand_is_rejected() {
        Cli::try_parse_from(["web-agent-research", "some goal"])
            .expect_err("goal without the research subcommand must not parse");
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
    fn events_flag_maps_dash_to_stderr_and_anything_else_to_a_file() {
        let events = |args: &[&str]| {
            let Command::Research { events, .. } = parse(args).command;
            events_sink(events)
        };
        assert_eq!(events(&["war", "research", "g"]), None);
        assert_eq!(
            events(&["war", "research", "g", "--events", "-"]),
            Some(EventsSink::Stderr)
        );
        assert_eq!(
            events(&["war", "research", "g", "--events", "out.ndjson"]),
            Some(EventsSink::File(PathBuf::from("out.ndjson")))
        );
    }

    #[test]
    fn events_detail_defaults_to_normal_and_accepts_tokens() {
        let detail = |args: &[&str]| {
            let Command::Research { events_detail, .. } = parse(args).command;
            events_detail.as_detail()
        };
        assert_eq!(detail(&["war", "research", "g"]), EventsDetail::Normal);
        assert_eq!(
            detail(&["war", "research", "g", "--events-detail", "tokens"]),
            EventsDetail::Tokens
        );
        assert!(Cli::try_parse_from(["war", "research", "g", "--events-detail", "all"]).is_err());
    }

    #[test]
    fn progress_defaults_to_auto_and_accepts_the_three_modes() {
        let progress = |args: &[&str]| {
            let Command::Research { progress, .. } = parse(args).command;
            progress.as_mode()
        };
        assert_eq!(progress(&["war", "research", "g"]), ProgressMode::Auto);
        for (value, mode) in [
            ("auto", ProgressMode::Auto),
            ("plain", ProgressMode::Plain),
            ("off", ProgressMode::Off),
        ] {
            assert_eq!(
                progress(&["war", "research", "g", "--progress", value]),
                mode
            );
        }
        assert!(Cli::try_parse_from(["war", "research", "g", "--progress", "fancy"]).is_err());
    }

    #[test]
    fn render_goes_to_stdout_shape() {
        let cite = |url: &str, support: Support| CitationDTO {
            url: url.to_owned(),
            title: Some("t".to_owned()),
            support,
            quote: None,
        };
        let response = ResearchResponse {
            session_id: "s".to_owned(),
            content_trust: CONTENT_TRUST_UNTRUSTED_WEB.to_owned(),
            synthesis: SynthesisDTO {
                size: web_agent_research::research::synthesis::SynthesisSize::Small,
                summary: "summary text".to_owned(),
                themes: vec![ThemeDTO {
                    title: "Findings".to_owned(),
                    points: vec!["point one".to_owned()],
                    citations: vec![],
                }],
                citations: vec![
                    cite("https://example.com/t", Support::Exact),
                    cite("https://example.com/u", Support::None),
                    cite("https://example.com/p", Support::Partial),
                ],
                verification: VerificationDTO::default(),
            },
            turns_used: 1,
            turn_budget: 10,
            evidence_urls: vec!["https://example.com/t".to_owned()],
            usage: UsageDTO {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                reasoning_tokens: 0,
                cached_prompt_tokens: 0,
                cache_creation_prompt_tokens: 0,
            },
        };
        let printed = render(&response);
        assert!(printed.contains("summary text"));
        assert!(printed.contains("Sources:"));
        assert!(
            printed.contains("1. [t](https://example.com/t)\n"),
            "{printed}"
        );
        assert!(
            printed.contains("2. [t](https://example.com/u) (unsupported:"),
            "{printed}"
        );
        assert!(
            printed.contains("3. [t](https://example.com/p) (partial:"),
            "{printed}"
        );
    }
}
