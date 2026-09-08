# Web Agent Research

CLI prototype of a multi-turn web research agent on the Obscura engine, evolving into an API.

## Language

**Research**:
User's research goal executed by the agent until final synthesis.
_Avoid_: query, task, job

**Session**:
Multi-turn execution of a Research, with recoverable turn history.
_Avoid_: conversation, thread, run

**Turn**:
One iteration of the plan → search → read → synthesize loop inside a Session.
_Avoid_: step, iteration, cycle

**Evidence**:
Web-extracted content with source URL and collection time, used in synthesis.
_Avoid_: source, document, snippet, chunk

**Mode**:
Execution shape of a Research: `search` (agent loop over search → fetch → synthesize) or `deep` (same loop with page interaction and re-planning).
_Avoid_: workflow, strategy

**Synthesis**:
Final explanatory summary of a Research's Evidence, sized `small`, `medium`, `large` or `deep`.
_Avoid_: report
