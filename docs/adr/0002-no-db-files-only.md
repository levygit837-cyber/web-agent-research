# No DB in the prototype: files only

A Session needs multi-turn recovery from day one, but without operating infrastructure; we decided on one JSONL file per Session (`sessions/<id>.jsonl`) plus plain files for other persisted state, and we migrate to SQLite when JSONL corruption or repeated work starts to hurt.

## Considered Options

- SQLite from day one — WAL, `sessions`/`turns`/`evidence` tables; a backup is one file, but it demands a schema and migrations before the loop is validated.
- Postgres + Redis — concurrent queries and native TTL; the cost is a mandatory docker-compose just to run the CLI, against the goal of simplicity.

## Consequences

- The Turn format in JSONL must migrate 1:1 to a future `turns` table (id, session_id, Evidence with URL + fetched_at); nothing throwaway.
- No fetch cache exists. The first version of this ADR planned an on-disk fetch cache keyed by normalized URL and validated by ETag/Last-Modified; it was never built, and [ADR-0006](0006-web-search-tool-architecture.md) defers it until repeat fetches are measured. Within one run, a repeat `fetch` of a page already read sends no request.
- Other persisted state is plain files under the cache root (`web::cache_dir`): the search-leg cache under `search/` and the engine governor state in `engines.json`. There is no eviction policy beyond a size cap and a TTL on the search-leg cache.
