# 0001 — Estilos de arquitetura aplicáveis a Rust

## Fontes

- Cockburn, artigo original Hexagonal/Ports & Adapters (2005): https://alistair.cockburn.us/hexagonal-architecture
- Martin, "The Clean Architecture" (post original do autor, 2012): https://blog.cleancoder.com/uncle-bob/2012/08/13/the-clean-architecture.html
- Bogard, "Vertical Slice Architecture" (artigo do autor, 2018): https://www.jimmybogard.com/vertical-slice-architecture/
- The Cargo Book, capítulo Workspaces: https://doc.rust-lang.org/cargo/reference/workspaces.html
- Rust API Guidelines, "About" (escopo das guidelines): https://rust-lang.github.io/api-guidelines/about.html
- Rust API Guidelines, "Flexibility" (C-GENERIC vs C-OBJECT: generics, trait objects, trade-offs): https://rust-lang.github.io/api-guidelines/flexibility.html
- Repo tokio (layout multi-crate, workspace members): https://github.com/tokio-rs/tokio
- Manifesto workspace do tokio (members, resolver, lints compartilhados): https://raw.githubusercontent.com/tokio-rs/tokio/master/Cargo.toml
- Repo axum (estrutura axum / axum-core / axum-extra / axum-macros): https://github.com/tokio-rs/axum
- Manifesto workspace do axum (`members = ["axum", "axum-*"]`, lints compartilhados): https://raw.githubusercontent.com/tokio-rs/axum/main/Cargo.toml

## Achados

### 1. Hexagonal / Ports & Adapters (Cockburn)

- Ideia central (3 linhas): a aplicação fica no "dentro", ignorante da natureza dos dispositivos externos; conversa com o "fora" apenas via ports (APIs que definem um diálogo com propósito); adapters convertem eventos/sinais de cada tecnologia (UI, teste, batch, HTTP, banco) para o protocolo do port (fonte: Cockburn).
- O artigo nasceu para impedir que lógica de negócio vaze para o código de UI e para permitir rodar a aplicação sem UI e sem banco, testável por scripts automatizados (fonte: Cockburn).
- O seam vive no port: o protocolo/API é definido pelo lado de dentro, e cada dispositivo externo pluga um adapter que se conforma a ele — "qualquer dispositivo que adira aos protocolos de um port pode ser plugado nele" (fonte: Cockburn).
- O que varia através do seam: a tecnologia externa (GUI vs harness FIT vs driver batch vs HTTP; SQL vs flat file vs mock in-memory), sem mudar a conversa funcional da aplicação (fonte: Cockburn).
- A ordem de construção sugerida é: primeiro harness de teste + mock in-memory no lugar do banco real, depois GUI ainda sobre o mock, só então integração contra banco real (fonte: Cockburn).
- Custo em Rust: o port vira `trait`; o adapter pode ser plugado por generics (static dispatch, monomorfização por tipo) ou por trait object `dyn` (heterogeneidade e menor tamanho de código, ao custo de indireção + vtable) — os dois regimes e seus trade-offs são documentados nas guidelines (fonte: API Guidelines Flexibility).
- Custo em Rust: generics duplicam o corpo da função por tipo concreto (aumento de code size) e tornam coleções homogêneas (`Vec<T>` = um único tipo concreto); trait objects permitem coleções heterogêneas mas proíbem métodos genéricos e `Self` fora da posição de receiver (fonte: API Guidelines Flexibility).
- Custo em Rust: a guideline recomenda decidir cedo se o trait será usado como bound de generics ou como objeto, e usar `where Self: Sized` para excluir métodos genéricos e manter o trait object-safe (fonte: API Guidelines Flexibility).
- Navegabilidade para agentes: o seam é explícito e grepável (`trait Port`, `impl Port for XAdapter`), mas cada port adiciona um nível de indireção que o agente precisa atravessar para achar o comportamento real [INFERENCE — julgamento de engenharia, sem fonte primária].

### 2. Clean Architecture / Layered (Martin)

- Ideia central (3 linhas): círculos concêntricos onde, quanto mais para dentro, mais alto-nível e abstrata a política; Entities (regras enterprise-wide) no centro, Use Cases (regras application-specific) ao redor, Interface Adapters convertendo formatos, Frameworks/Drivers (UI, banco, web) na borda externa (fonte: Martin).
- A regra que sustenta tudo é a Dependency Rule: dependências de código-fonte só apontam para dentro; nada num círculo interno pode mencionar qualquer nome (função, classe, variável) declarado num círculo externo (fonte: Martin).
- O seam vive nas fronteiras entre círculos, cruzadas por Dependency Inversion: o use case chama uma interface (Output Port) declarada no círculo interno, e o presenter/adapter do círculo externo a implementa — a dependência de código-fonte opõe-se ao fluxo de controle (fonte: Martin).
- O que varia através do seam: o mecanismo externo inteiro (trocar Web UI por console UI, Oracle por Mongo, framework web por outro) sem tocar nas regras de negócio (fonte: Martin).
- O que cruza a fronteira são sempre estruturas de dados simples no formato mais conveniente para o círculo interno; passar Entities ou linhas do banco (RowStructure) para dentro viola a Dependency Rule (fonte: Martin).
- Os quatro círculos são esquemáticos, não obrigatórios — pode haver mais camadas, desde que a Dependency Rule seja preserved (fonte: Martin).
- Custo em Rust: Clean exige mais traits de inversão por fronteira (input port + output port por use case) do que Hexagonal puro, logo multiplica a escolha generics-vs-`dyn` por camada; cada fronteira genérica adicional é mais um ponto de monomorfização (code size) documentado nas guidelines (fonte: Martin + API Guidelines Flexibility).
- Custo em Rust: a proibição de mencionar tipos externos no núcleo implica mapear DTOs do framework (ex.: extractors do Axum, rows do driver) para structs do domínio em cada travessia — custo de alocação/cópia recorrente, evitável só com borrow cuidadoso (caller-control: quem precisa de ownership recebe ownership, quem não precisa recebe borrow) (fonte: Martin + API Guidelines Flexibility C-CALLER-CONTROL).
- Crítica secundária: Bogard relata que, na prática, arquiteturas em camadas/onion degeneram em cadeia rígida Controller→Service→Repository com mocks pesados e regras de dependência raramente úteis — opinião de blog, marcar como secundária, não fato (fonte secundária: Bogard).

### 3. Modular Monolith com workspaces/crates

- Ideia central (3 linhas): um único repositório/deploy dividido em crates com fronteiras de compilação reais; cada crate tem API pública explícita (`pub`) e dependências declaradas no manifesto; o workspace compartilha lockfile, diretório de build, resolver, lints e metadados (fonte: Cargo Book Workspaces).
- O workspace é definido pela seção `[workspace]` no `Cargo.toml` raiz, em dois sabores: com root package ou manifesto virtual (só `[workspace]`, sem `[package]`, com `resolver` explícito) para quando não há um crate "primário" (fonte: Cargo Book Workspaces).
- Membros são declarados em `members` (aceita globs como `crates/*`), com `exclude` para exceções; dependências `path` dentro do diretório do workspace viram membros automaticamente (fonte: Cargo Book Workspaces).
- Todos os membros compartilham um único `Cargo.lock` na raiz, um único diretório `target`, e as seções `[patch]`/`[replace]`/`[profile.*]` só são reconhecidas no manifesto raiz (fonte: Cargo Book Workspaces).
- Herança de metadados (`[workspace.package]`, `[workspace.dependencies]`, `[workspace.lints]`) com `*.workspace = true` nos membros elimina divergência de versão/edition/lints entre crates (fonte: Cargo Book Workspaces).
- Evidência de campo: o workspace tokio lista `tokio`, `tokio-macros`, `tokio-test`, `tokio-stream`, `tokio-util` (+ crates internas) com `resolver = "2"` e lints `[workspace.lints.rust]` compartilhados (fonte: manifesto workspace do tokio).
- Evidência de campo: o workspace axum usa `members = ["axum", "axum-*"]` com `rust-version`, `unsafe_code = "forbid"` e lints clippy/rust herdados — o núcleo (`axum-core`) é separado de extras (`axum-extra`) e macros (`axum-macros`) por fronteira de crate, não de camada abstrata (fonte: manifesto workspace do axum + repo axum).
- O seam vive na fronteira de crate: visibilidade (`pub` vs privado) + dependência declarada no `Cargo.toml` do membro; o que varia através dele é a implementação inteira do módulo, desde que a API pública seja mantida (fonte: Cargo Book Workspaces).
- Custo em Rust: fronteira de crate é checada pelo compilador (dependência ausente = não compila) sem custo de runtime — zero `dyn`, zero vtable; o custo é granularidade de compilação (mudança num crate rebuilda dependentes) e disciplina de API pública (fonte: Cargo Book Workspaces + [INFERENCE] sobre rebuild, comportamento padrão do cargo).
- Navegabilidade para agentes: estrutura espelha o mapa mental (um diretório = um módulo de domínio); `cargo check -p <crate>` / `cargo test -p <crate>` dão escopo barato por fatia sem carregar o workspace inteiro (fonte: Cargo Book Workspaces — comandos rodam por membro via `-p`/`--workspace`).

### 4. Vertical Slice

- Ideia central (3 linhas): em vez de fatiar por camada técnica, fatia por request/caso de uso, agrupando todas as preocupações (input, validação, lógica, persistência, resposta) de ponta a ponta; acoplamento é maximizado dentro da slice e minimizado entre slices; cada slice escolhe o padrão de lógica mais simples que resolve seu caso (fonte primária do autor: Bogard).
- O artigo nasce da experiência do autor: após meses num projeto onion, "as rachaduras começaram a aparecer" e a equipe migrou para CQRS + slices verticais, abordagem que usa com exclusividade há 7–8 anos — relato de experiência do autor, não estudo controlado (fonte: Bogard).
- O seam vive entre slices: não há gates/barreiras entre camadas dentro da slice; a fronteira que importa é a lateral, entre um request e outro (fonte: Bogard).
- O que varia através do seam: o padrão de lógica por slice (começar com Transaction Script e refatorar para o padrão que emergir dos code smells, por slice, sem decisão application-wide) e o caminho de dados (queries podem ler direto, commands passam pelo domínio) (fonte: Bogard, que remete aos Domain Logic patterns de Fowler).
- Com slices, a maioria das abstrações compartilhadas "derrete": sem camada obrigatória de services/repositories genéricos; o compartilhamento cross-slice é mantido no mínimo (fonte: Bogard).
- Custo em Rust: sem traits de fronteira por definição, logo sem custo `dyn`/generics dentro da slice — o código chama funções/structs concretos; o custo aparece se duas slices precisarem do mesmo comportamento e ele for duplicado em vez de extraído para um módulo compartilhado (fonte: Bogard + [INFERENCE] sobre trade-off duplicação-vs-extração).
- Custo em Rust: em Rust a slice tende a ser um módulo (`mod`) com handler + tipos de request/response + lógica colados; extrair o miolo compartilhado depois é mover código para um módulo/crate comum — barato porque o compilador aponta todos os usos (fonte: [INFERENCE] — julgamento de engenharia, sem fonte primária).
- Pré-requisito explícito do autor: a equipe precisa dominar code smells e refatoração (saber quando empurrar lógica para o domínio); sem isso, o estilo não é recomendado (fonte: Bogard).
- Navegabilidade para agentes: mudança de feature = tocar arquivos de uma slice só, sem atravessar camadas; risco inverso é divergência silenciosa entre slices duplicadas, que exige grep ativo por lógica gêmea [INFERENCE — julgamento de engenharia, sem fonte primária].

## Implicação p/ web-agent-research

- Decisão: Modular Monolith com workspace Cargo como esqueleto — `src/lib.rs` (núcleo: Pesquisa/Sessão/Turno/Evidência, cf. CONTEXT.md) + binário CLI fino (`src/main.rs`: parse args → chama lib) hoje, crate/handler Axum futuro consumindo a mesma lib. Espelha tokio/axum (núcleo separado de adapters por fronteira de crate) e prepara `POST /turn` sem rewrite da lógica.
- Ports hexagonais mínimos dentro da lib, só onde há variação real e conhecida: `Engine::fetch` (subprocesso Obscura hoje, sessão CDP persistente amanhã) e gateway LLM (OpenAI-compatible hoje, outro provider depois). Nada de Output Port por use case estilo Clean — overkill para um protótipo com dois adapters.
- Organização interna da lib por Vertical Slice do Turno (planejar → buscar → ler → sintetizar): handler + tipos + validação colados por Turno; extrair domínio compartilhado (Sessão, Evidência, persistência JSONL) só quando o segundo Turno-tipo exigir. Evita a cadeia rígida Controller→Service→Repository criticada por Bogard.
- Regra anti-custo: concreto por padrão dentro da lib; `trait` só no port do Engine/gateway; `dyn` só se for preciso heterogeneidade real (ex.: lista de engines mista), nunca por antecipação — conforme trade-offs C-GENERIC/C-OBJECT das guidelines. DTOs de borda (args CLI, JSON HTTP) mapeados para tipos do núcleo na travessia, sem vazar tipos do framework para dentro (Dependency Rule de Martin aplicada só na borda, não em 4 camadas).
- O que NÃO fazer: Clean em 4 círculos com Entities/Use-Cases/Adapters genéricos desde o dia 1 — para CLI-fino + lib-núcleo + futura API Axum, o custo (mapeamentos por fronteira, traits de inversão por use case, indireção para o agente navegar) supera o benefício; reavaliar apenas se o número de adapters externos dobrar.
