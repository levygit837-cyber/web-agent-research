//! What Progress knows of the reply in flight (#154): the tail of the text
//! forming, and counters for thinking and tool-call arguments. The state
//! lives for one gateway attempt; a `model.retry`, a new Turn or the reply
//! itself resets it, so a fragment of a failed attempt is never shown next
//! to a later one and nothing partial reaches a permanent line.
//!
//! Only the last `tail_max` characters of the text are kept, in a buffer
//! reused across fragments, so a long answer costs no more than a short one.

use super::super::model::{Channel, Delta};

#[derive(Default)]
pub(super) struct Partial {
    attempt: u32,
    /// Characters of answer text seen; more than the tail means it was cut.
    text_chars: usize,
    /// The end of the text, control characters flattened to spaces.
    tail: String,
    thinking_chars: usize,
    args_chars: usize,
    /// The tool the latest fragment named, once the model has said it.
    args_name: Option<String>,
    /// What the latest fragment was: the indicator shows that stream.
    latest: Option<Channel>,
}

impl Partial {
    pub(super) fn reset(&mut self) {
        self.attempt = 0;
        self.text_chars = 0;
        self.tail.clear();
        self.thinking_chars = 0;
        self.args_chars = 0;
        self.args_name = None;
        self.latest = None;
    }

    /// Fold one fragment in. `tail_max` bounds the text kept.
    pub(super) fn push(&mut self, delta: &Delta<'_>, tail_max: usize) {
        if delta.attempt != self.attempt {
            self.reset();
            self.attempt = delta.attempt;
        }
        let chars = delta.text.chars().count();
        match delta.channel {
            Channel::Text => {
                self.text_chars += chars;
                self.tail.extend(
                    delta
                        .text
                        .chars()
                        .map(|c| if c.is_control() { ' ' } else { c }),
                );
                let excess = self.tail.chars().count().saturating_sub(tail_max);
                if excess > 0 {
                    let cut = self
                        .tail
                        .char_indices()
                        .nth(excess)
                        .map_or(self.tail.len(), |(at, _)| at);
                    self.tail.drain(..cut);
                }
            }
            Channel::Thinking => self.thinking_chars += chars,
            Channel::ToolArgs => {
                self.args_chars += chars;
                if let Some(name) = delta.name.filter(|name| !name.is_empty()) {
                    if self.args_name.as_deref() != Some(name) {
                        self.args_name = Some(name.to_owned());
                    }
                }
            }
        }
        self.latest = Some(delta.channel);
    }

    /// The indicator for the latest stream. The text tail is cut from the
    /// left to fit `room` characters, so the newest words stay visible.
    /// `None` before any fragment.
    pub(super) fn indicator(&self, room: usize) -> Option<String> {
        Some(match self.latest? {
            Channel::Text => {
                const LABEL: &str = "model writing: ";
                let width = room.saturating_sub(LABEL.len() + 3).max(8);
                let tail_len = self.tail.chars().count();
                if tail_len <= width && self.text_chars == tail_len {
                    format!("{LABEL}{}", self.tail)
                } else {
                    let tail: String = self
                        .tail
                        .chars()
                        .skip(tail_len.saturating_sub(width))
                        .collect();
                    format!("{LABEL}...{tail}")
                }
            }
            Channel::Thinking => format!("model thinking... {} chars", self.thinking_chars),
            Channel::ToolArgs => {
                let name = self.args_name.as_deref().unwrap_or("tool");
                format!("model writing {name} args... {} chars", self.args_chars)
            }
        })
    }
}
