# Two LLM wire formats as an enum branch inside `llm::Gateway`

Amends ADR-0006 (the "no `trait` until a second adapter exists" rule now has its second adapter).

`llm::` spoke only OpenAI Chat Completions. Anthropic's OpenAI-compatible layer drops prompt caching and exposes `thinking` only through extra body, so we add the native Messages API (#47). That is the second adapter ADR-0006 was waiting for. We decide it is an `ApiFormat` enum (`Openai`, `Anthropic`) chosen by `GATEWAY_API_FORMAT`, with one `match` in `Gateway` for the URL, the auth headers, the body builder, and the response parser. We do not add a `trait`.

## Decision

1. `Gateway` keeps its public seam unchanged (`new`, `chat`, `chat_with_tools`, `GatewayConfig::from_env`). `research` never learns which format is active.
2. `llm/anthropic.rs` owns the Messages wire shape: request mapping from the format-neutral `ChatMessage` transcript, `cache_control` breakpoints, and the response parser into `LlmReply`.
3. Provider blocks that must round-trip verbatim (Anthropic `thinking`/`redacted_thinking` with `signature`) ride as opaque `ReplayBlocks` on `LlmReply` and `ChatMessage`. `research` stores and forwards them without looking inside.
4. Retry, backoff, and status mapping stay shared. A 403 is now `GatewayError::Forbidden(<upstream message>)`, separate from 401 `Auth`.

## Considered options

- `trait LlmWire` with two impls: rejected for now. Two formats with the same retry loop and the same four-item seam, known at config time, fit a closed enum. A `trait` adds a dynamic-dispatch seam with no third implementation in sight.
- Anthropic through its OpenAI-compatible layer: rejected. No prompt caching, and no first-class thinking blocks.
- A separate `AnthropicGateway` type: rejected. `research` would have to choose between types, which breaks the rule that it never learns the format.

## Consequences

- A third wire format should revisit the `trait` question instead of adding a third `match` arm by default.
- `TokenUsage` and the Harness `usage` object gain `cache_creation_prompt_tokens` (`#[serde(default)]` in Session rows, so `SESSION_FORMAT_VERSION` stays 1).
