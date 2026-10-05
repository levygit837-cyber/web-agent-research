# Sem DB no protótipo — só arquivos (JSONL + cache HTTP em disco)

Sessão precisa de recuperação multi-turno desde o dia 1, mas sem operar infra; decidimos JSONL por sessão (`sessions/<id>.jsonl`) + cache de fetch em disco por URL normalizada, e migramos para SQLite quando repetição de fetch ou corrupção de JSONL doer.

## Considered Options

- SQLite + cache em disco desde o dia 1 — WAL, tabelas sessions/turns/evidence + fetch_cache; backup é um arquivo, mas exige schema/migrations antes de validar o loop.
- Postgres + Redis — queries concorrentes e TTL nativo; custo é docker-compose obrigatório até para rodar o CLI, contra a meta de simplicidade.

## Consequences

- Formato do turno em JSONL deve ser migrável 1:1 para futura tabela `turns` (id, session_id, evidências com URL + fetched_at); nada de formato throwaway.
- Sem TTL/evicção inteligente no início: cache é keyada por URL normalizada + validação por ETag/Last-Modified quando presente.

## Where Sessions live (amended by #102)

Sessions are user data, not disposable cache, and a Harness runs the CLI from someone else's project, so they never go under the current directory. They live under a per-user data root, resolved once: `$WEB_AGENT_RESEARCH_HOME`, else `$XDG_DATA_HOME/web-agent-research`, else `$HOME/.local/share/web-agent-research`, in `<root>/sessions/<id>.jsonl`. The search cache and `engines.json` are disposable and stay under the cache root (`web::cache_dir`: `SEARCH_CACHE_DIR`, else `$XDG_CACHE_HOME/web-agent-research`, else `$HOME/.cache/web-agent-research`). Two roots, because the XDG split exists for this: deleting the cache must never lose a Session.

- The defaulted path is best-effort: a write failure warns on stderr and the run still exits 0, since the Synthesis is already paid for. An explicit `--session-out` is a caller contract and still exits 6 on failure.
- `--session-id` becomes a file name, so it must match `[A-Za-z0-9._-]{1,64}` with no leading dot (exit 2 otherwise).
- Files are created with mode 0600 (directories 0700) on unix, and the sessions directory is capped at `SESSION_MAX_BYTES` (default 256 MiB), evicting the oldest files first. The Session just written is never evicted; explicit paths are never touched.
