# Pipeline por Turno em monólito modular, ports mínimos

Pesquisa multi-turno (classificar → modo → multi-query → ranquear → extrair → interagir → sintetizar) precisa de um mapa que um agente consiga navegar sem atravessar camadas, e que vire API sem rewrite; decidimos monólito modular em package único hoje (lib-núcleo + CLI fino), organizado por dentro em slices do Turno, com só dois traits de fronteira (`Engine::fetch`, gateway LLM), evoluindo para workspace Cargo quando o handler Axum/MCP chegar.

## Considered Options

- Hexagonal full com port por caso de uso — isola cada tecnologia externa atrás de trait próprio; custo é indireção por toda parte para um protótipo com só 2 adapters reais (ver `docs/research/arquitetura/0001-estilos-rust.md`).
- Clean em 4 círculos (Entities/Use Cases/Adapters/Frameworks) — Dependency Rule estrita; custo é mapear DTOs em cada travessia e multiplicar traits de inversão, sem pagar de volta no dia 1.
- Modular Monolith com workspace desde já (`crates/nucleo`, `crates/cli`, `crates/api`) — fronteira de compilação real; custo é cerimônia de workspace antes de existir o segundo binário.
- Vertical Slice puro sem núcleo compartilhado — cada Modo com seu handler colado; custo é duplicar Sessão/Evidência entre `single` e `deep` logo no segundo modo.

## Consequences

- `src/lib.rs` declara o núcleo: `pipeline` (loop do Turno + estágios), `session` (Sessão/Turno/Evidência + JSONL), `engine` (trait `Engine::fetch`: subprocesso Obscura hoje, CDP persistente amanhã), `llm` (gateway OpenAI-compatible), `tool` (seam harness: tool-result JSON hoje, MCP STDIO depois).
- `src/main.rs` continua casca fina (parse args → chama lib); futura API Axum vira membro de workspace consumindo a mesma lib — zero rewrite.
- Concreto por padrão; `trait` só em `engine`/`llm`; `dyn` só com heterogeneidade real. DTOs de borda (args CLI, JSON HTTP) mapeados para tipos do núcleo na travessia, sem vazar tipos do framework para dentro.
- Workspace Cargo (`crates/api`, `crates/mcp`) só quando o segundo binário existir; até lá, package único.
- Evidências: `docs/research/arquitetura/0001-estilos-rust.md`, `0002-pipeline-agente.md`, `0003-runtime-obsura-tokio-axum.md`.
