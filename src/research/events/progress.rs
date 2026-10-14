//! Progress (#149, ADR-0010 §6): the human view of a Session on stderr.
//!
//! [`Progress`] is one more concrete output of the emitter state, fed by the
//! same `emit` as the NDJSON writer and the Session log: it renders from the
//! Event `type` and `data` documented in `docs/harness.md`, never from a
//! second instrumentation path. The text is for people and is not a contract;
//! Harnesses read `--events`.
//!
//! Two shapes, chosen once per Session by [`ProgressMode::style`]:
//! - [`Style::Plain`]: one permanent line per meaningful Event, no ANSI.
//! - [`Style::Tty`]: the same lines (optionally coloured), plus one
//!   transient status line for the step in flight. The transient line is
//!   redrawn on each Event, never by a timer, so no thread competes for the
//!   emitter lock. It is cleared before anything else is written to stderr
//!   and when the Session finishes.
//!
//! On a terminal the transient line also follows the reply as it streams
//! (#154): the tail of the answer text forming, or a character count while the
//! model writes tool arguments or thinks. Fragments only ever replace that
//! line; a `model.retry`, the next Turn and the reply itself drop them, so no
//! partial text reaches a permanent line. `plain` and piped Progress take no
//! fragments at all.
//!
//! Meaning never rests on colour: failures say `failed`, outcomes are words.

use std::io::{IsTerminal, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::model::Delta;
use super::EventsSink;

mod lines;
mod stream;
#[cfg(test)]
mod tests;

use lines::Tone;

/// `ESC [ 2 K` after `CR`: erase the transient status line.
pub const CLEAR_LINE: &str = "\r\x1b[2K";

/// The transient line is clipped to this many characters so it never wraps:
/// a wrapped line would leave its first row behind when it is cleared.
const STATUS_MAX_CHARS: usize = 70;

/// `--progress`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressMode {
    /// Lines plus a live status line when stderr is a terminal; nothing
    /// otherwise, so a Harness that pipes stderr sees what it always saw.
    #[default]
    Auto,
    /// Lines without ANSI codes, even when stderr is piped.
    Plain,
    /// Nothing.
    Off,
}

/// How a Session renders Progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Style {
    Plain,
    Tty { color: bool },
}

impl ProgressMode {
    /// For serde: `auto` is the default and stays out of a serialized request.
    pub fn is_auto(&self) -> bool {
        *self == Self::Auto
    }

    /// The decision, with every input explicit so tests need no terminal.
    /// `--events -` owns stderr, so it turns Progress off in every mode: one
    /// stderr, one format.
    pub(super) fn style(
        self,
        events: Option<&EventsSink>,
        stderr_is_tty: bool,
        no_color: bool,
    ) -> Option<Style> {
        if matches!(events, Some(EventsSink::Stderr)) {
            return None;
        }
        match self {
            Self::Off => None,
            Self::Plain => Some(Style::Plain),
            Self::Auto => stderr_is_tty.then_some(Style::Tty { color: !no_color }),
        }
    }

    /// Whether the Session keeps a transient status line on stderr. The
    /// binary clears it ([`CLEAR_LINE`]) before a `tracing` line is written.
    pub fn draws_status_line(self, events: Option<&EventsSink>, stderr_is_tty: bool) -> bool {
        matches!(
            self.style(events, stderr_is_tty, false),
            Some(Style::Tty { .. })
        )
    }
}

/// The Progress renderer of one Session. See the module docs.
pub(super) struct Progress {
    /// `None` once a write failed: the output is off for the rest of the
    /// Session and Progress never changes the exit code.
    out: Option<Box<dyn Write + Send>>,
    style: Style,
    turn_budget: u32,
    /// The grounding counts of `answer.verified`, repeated on the last line.
    verification: Option<String>,
    /// The step in flight, shown on the transient line.
    status: Option<String>,
    /// A transient line is on screen and must be erased before any write.
    drawn: bool,
    /// The reply in flight, for the transient line (terminal only).
    partial: stream::Partial,
}

impl Progress {
    /// Progress on the process stderr, as the CLI runs it.
    pub(super) fn for_stderr(mode: ProgressMode, events: Option<&EventsSink>) -> Option<Self> {
        let stderr = std::io::stderr();
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
        let style = mode.style(events, stderr.is_terminal(), no_color)?;
        Some(Self::new(style, Box::new(stderr)))
    }

    pub(super) fn new(style: Style, out: Box<dyn Write + Send>) -> Self {
        Self {
            out: Some(out),
            style,
            turn_budget: 0,
            verification: None,
            status: None,
            drawn: false,
            partial: stream::Partial::default(),
        }
    }

    /// Whether this renderer draws fragments: only a terminal does.
    pub(super) fn wants_deltas(&self) -> bool {
        matches!(self.style, Style::Tty { .. })
    }

    /// Redraw the transient line with the reply so far. One `write`, no
    /// permanent line: the fragment lives on the status line alone.
    pub(super) fn delta(&mut self, turn: u32, delta: &Delta<'_>) {
        if !self.wants_deltas() {
            return;
        }
        let label = format!("{}: ", self.turn_label(turn));
        let room = STATUS_MAX_CHARS.saturating_sub(label.chars().count());
        self.partial.push(delta, room);
        let Some(indicator) = self.partial.indicator(room) else {
            return;
        };
        let status = lines::clip(&format!("{label}{indicator}"), STATUS_MAX_CHARS);
        let mut out = String::new();
        if std::mem::take(&mut self.drawn) {
            out.push_str(CLEAR_LINE);
        }
        out.push_str(&self.paint(&status, "2"));
        self.status = Some(status);
        self.drawn = true;
        self.write(&out);
    }

    /// Erase the transient line, if one is showing. `emit` calls this before
    /// it writes the other outputs, whose own warnings share stderr.
    pub(super) fn clear(&mut self) {
        if std::mem::take(&mut self.drawn) {
            self.write(CLEAR_LINE);
        }
    }

    /// Render one Event: its permanent line (if any), then the transient
    /// status of the step it starts (if any). One `write` per Event.
    pub(super) fn render(&mut self, turn: Option<u32>, kind: &str, elapsed_ms: u64, data: &Value) {
        match kind {
            "turn.started" => {
                self.turn_budget =
                    u32::try_from(lines::count_of(data, "turn_budget")).unwrap_or(u32::MAX);
                self.partial.reset();
            }
            "model.retry" | "model.replied" => self.partial.reset(),
            "answer.verified" => self.verification = Some(lines::verification_counts(data)),
            _ => {}
        }
        let line = lines::describe(kind, data, self.verification.as_deref());
        let in_flight = lines::in_flight(kind, data);
        let mut out = String::new();
        if std::mem::take(&mut self.drawn) {
            out.push_str(CLEAR_LINE);
        }
        if let Some((text, tone)) = &line {
            let prefix = self.prefix(turn, elapsed_ms);
            out.push_str(&self.paint(&prefix, "2"));
            out.push(' ');
            out.push_str(&self.paint(text, tone.code()));
            out.push('\n');
        }
        // A kind that yields neither (`model.delta`, unknown types) leaves
        // the status as it was.
        if let Some(text) = in_flight {
            let label = turn.map_or_else(String::new, |turn| self.turn_label(turn) + ": ");
            self.status = Some(lines::clip(&format!("{label}{text}"), STATUS_MAX_CHARS));
        } else if line.is_some() {
            self.status = None;
        }
        if kind == "session.finished" {
            self.status = None;
        }
        if matches!(self.style, Style::Tty { .. }) {
            if let Some(status) = &self.status {
                out.push_str(&self.paint(status, "2"));
                self.drawn = true;
            }
        }
        if !out.is_empty() {
            self.write(&out);
        }
    }

    fn turn_label(&self, turn: u32) -> String {
        if self.turn_budget > 0 {
            format!("turn {turn}/{}", self.turn_budget)
        } else {
            format!("turn {turn}")
        }
    }

    fn prefix(&self, turn: Option<u32>, elapsed_ms: u64) -> String {
        let seconds = elapsed_ms as f64 / 1000.0;
        match turn {
            Some(turn) => format!("[{} - {seconds:.1}s]", self.turn_label(turn)),
            None => format!("[{seconds:.1}s]"),
        }
    }

    /// `text` in SGR `code` when colour is on; otherwise unchanged.
    fn paint(&self, text: &str, code: &str) -> String {
        match self.style {
            Style::Tty { color: true } => format!("\x1b[{code}m{text}\x1b[0m"),
            _ => text.to_owned(),
        }
    }

    fn write(&mut self, text: &str) {
        let Some(out) = self.out.as_mut() else {
            return;
        };
        if let Err(err) = out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
            // Same rule as the NDJSON output: say so once and stop.
            eprintln!("warning: progress not written: {err}");
            self.out = None;
        }
    }
}

impl Tone {
    /// SGR code of the tone; the words in the line carry the meaning too.
    fn code(self) -> &'static str {
        match self {
            Self::Normal => "0",
            Self::Good => "32",
            Self::Bad => "31",
            Self::Warn => "33",
        }
    }
}
