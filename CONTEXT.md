# Web Agent Research

CLI Web Search tool: a Harness asks one question, a multi-turn research agent searches and fetches the web, and only the Synthesis returns.

## Language

**Web Search**:
The single public tool this repo ships: `web-agent-research research "<goal>" --json`, one Research in, one Synthesis out.
_Avoid_: search tool (collides with the internal `search` capability), API

**Harness**:
The external coding-agent host (omp, Claude Code, Codex) that invokes Web Search on behalf of its own model.
_Avoid_: client, caller, host

**Hit**:
One search-engine result (title, URL, snippet). A candidate to fetch, never cited as-is.
_Avoid_: result, source

**Research**:
User's research goal executed by the agent until final synthesis.
_Avoid_: query, task, job

**Session**:
Multi-turn execution of a Research, with recoverable turn history.
_Avoid_: conversation, thread, run

**Turn**:
One iteration of the plan → search → fetch → synthesize loop inside a Session.
_Avoid_: step, iteration, cycle

**Evidence**:
Web-extracted content with source URL and collection time, used in synthesis.
_Avoid_: source, document, snippet, chunk

**Mode**:
Execution shape of a Research. Only `search` exists (agent loop over search → fetch → synthesize); `deep` (page interaction, re-planning) is not built.
_Avoid_: workflow, strategy

**Synthesis**:
Final explanatory summary of a Research's Evidence, sized `small`, `medium`, `large` or `deep`.
_Avoid_: report
