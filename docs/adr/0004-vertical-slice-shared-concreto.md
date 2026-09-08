# Vertical Slice com shared/ concreto, sem traits

Pesquisa cresce por Modo (`single` → `paralelo` → `deep`), não por tecnologia externa: Obscura é engine única (subprocesso hoje, portar código amanhã — mesma cara, sem 2º adapter real) e gateway LLM e persistência em arquivos já estão travados nos ADRs 0002/0003; decidimos 1 pasta por Modo com lógica colada e `shared/` só para o que 2+ slices usam (`fetch`, Sessão, Evidência, cache), com funções concretas e zero `trait` até existir variação real.

## Considered Options

- Hexagonal com ports `Engine`/`Llm` — isola tecnologia externa atrás de `trait`; descartado porque trocar Obscura não é opção (sem 2º adapter o port é Seam hipotético).
- Clean em camadas (entities/use_cases/adapters) — núcleo puro com Dependency Rule; descartado porque cobra conversão de tipo em cada travessia e 4 lugares por feature para um protótipo com 1 CLI e arquivos.
- Vertical Slice puro sem `shared/` — cada Modo com seu `fetch` colado; descartado porque `fetch`/Sessão/Evidência servem aos 3 Modos e duplicariam.

## Consequences

- `src/shared/` guarda Sessão/Turno/Evidência + JSONL, `fetch` Obscura, gateway OpenAI-compatible e cache em disco; `src/slices/<modo>/handler.rs` guarda 1 Modo de ponta a ponta.
- Concreto por padrão; `trait` só se surgir 2º adapter real (extração guiada pelo compilador).
- Reavaliar Clean apenas se adapters externos dobrarem (ex.: 5 front-ends + 3 bancos).
