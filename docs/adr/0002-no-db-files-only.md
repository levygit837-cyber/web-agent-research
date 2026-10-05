# No DB in the prototype: files only

A Session needs multi-turn recovery from day one, but without operating infrastructure; we decided on one JSONL file per Session (`sessions/<id>.jsonl`) plus plain files for other persisted state, and we migrate to SQLite when JSONL corruption or repeated work starts to hurt.

## Considered Options

- SQLite from day one — WAL, `sessions`/`turns`/`evidence` tables; a backup is one file, but it demands a schema and migrations before the loop is validated.
- Postgres + Redis — concurrent queries and native TTL; the cost is a mandatory docker-compose just to run the CLI, against the goal of simplicity.

## Consequences

- The Turn format in JSONL must migrate 1:1 to a future `turns` table (id, session_id, Evidence with URL + fetched_at); nothing throwaway.
- No fetch cache exists. The first version of this ADR planned an on-disk fetch cache keyed by normalized URL and validated by ETag/Last-Modified; it was never built, and [ADR-0006](0006-web-search-tool-architecture.md) defers it until repeat fetches are measured. Within one run, a repeat `fetch` of a page already read sends no request.
- Other persisted state is plain files under the cache root (`web::cache_dir`): the search-leg cache under `search/` and the engine governor state in `engines.json`. There is no eviction policy beyond a size cap and a TTL on the search-leg cache.

## Where Sessions live (amended by #102)

Sessions are user data, not disposable cache, and a Harness runs the CLI from someone else's project, so they never go under the current directory. They live under a per-user data root, resolved once: `$WEB_AGENT_RESEARCH_HOME`, else `$XDG_DATA_HOME/web-agent-research`, else `$HOME/.local/share/web-agent-research`, in `<root>/sessions/<id>.jsonl`. The search cache and `engines.json` are disposable and stay under the cache root (`web::cache_dir`: `SEARCH_CACHE_DIR`, else `$XDG_CACHE_HOME/web-agent-research`, else `$HOME/.cache/web-agent-research`). Two roots, because the XDG split exists for this: deleting the cache must never lose a Session.

- The defaulted path is best-effort: a write failure warns on stderr and the run still exits 0, since the Synthesis is already paid for. An explicit `--session-out` is a caller contract and still exits 6 on failure.
- `--session-id` becomes a file name, so it must match `[A-Za-z0-9._-]{1,64}` with no leading dot (exit 2 otherwise).
- Files are created with mode 0600 (directories 0700) on unix, and the sessions directory is capped at `SESSION_MAX_BYTES` (default 256 MiB), evicting the oldest files first. The Session just written is never evicted; explicit paths are never touched.
