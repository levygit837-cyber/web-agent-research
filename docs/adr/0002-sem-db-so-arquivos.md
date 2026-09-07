# Sem DB no protótipo — só arquivos (JSONL + cache HTTP em disco)

Sessão precisa de recuperação multi-turno desde o dia 1, mas sem operar infra; decidimos JSONL por sessão (`sessions/<id>.jsonl`) + cache de fetch em disco por URL normalizada, e migramos para SQLite quando repetição de fetch ou corrupção de JSONL doer.

## Considered Options

- SQLite + cache em disco desde o dia 1 — WAL, tabelas sessions/turns/evidence + fetch_cache; backup é um arquivo, mas exige schema/migrations antes de validar o loop.
- Postgres + Redis — queries concorrentes e TTL nativo; custo é docker-compose obrigatório até para rodar o CLI, contra a meta de simplicidade.

## Consequences

- Formato do turno em JSONL deve ser migrável 1:1 para futura tabela `turns` (id, session_id, evidências com URL + fetched_at); nada de formato throwaway.
- Sem TTL/evicção inteligente no início: cache é keyada por URL normalizada + validação por ETag/Last-Modified quando presente.
