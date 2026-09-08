# Pipeline de agente de research multi-turno

## Fontes

- Anthropic — Building Effective Agents (workflows: routing, prompt chaining, parallelization, orchestrator-workers, evaluator-optimizer; agents como LLM usando tools em loop com ground truth do ambiente): https://www.anthropic.com/engineering/building-effective-agents
- Anthropic — How we built our multi-agent research system (lead agent + subagentes paralelos, escala de esforço por complexidade, start-wide-then-narrow, síntese + CitationAgent, checkpoints/retries, rainbow deploys): https://www.anthropic.com/engineering/multi-agent-research-system
- Anthropic — Tool use overview (client tools vs server tools, `tool_use`/`tool_result`, `stop_reason: tool_use`): https://platform.claude.com/docs/en/agents-and-tools/tool-use/overview
- Anthropic — Handle tool calls (`tool_use_id`/`tool_result`/`is_error`, formatação e retry 2–3x, conteúdo não-confiável em `tool_result`): https://platform.claude.com/docs/en/agents-and-tools/tool-use/handle-tool-calls
- Anthropic — Web search tool (quando buscar, `max_uses`, dynamic filtering via code execution, domínios): https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool
- Anthropic — Web fetch tool (server tool, quando buscar, `max_uses`/`allowed_domains`, validação de URL, sem JS dinâmico → browser use): https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-fetch-tool
- OpenAI — Function calling / tool calling (tools, tool calls, tool outputs; loop 5 passos; execução no lado da aplicação): https://developers.openai.com/api/docs/guides/function-calling
- OpenAI — MCP and Connectors (tool `mcp` via Responses API, `server_url`/`server_label`, `require_approval`): https://developers.openai.com/api/docs/guides/tools-connectors-mcp
- MCP — What is MCP (standard aberto conectando AI apps a tools/recursos/workflows; STDIO local vs Streamable HTTP remoto): https://modelcontextprotocol.io/docs/2026-07-28/getting-started/intro
- MCP — Architecture (host/client/server, data layer JSON-RPC 2.0, stateless com `server/discover`, transports): https://modelcontextprotocol.io/docs/2026-07-28/learn/architecture
- MCP — Server concepts (Tools model-controlled, Resources application-driven, Prompts user-controlled; `tools/list` + `tools/call`): https://modelcontextprotocol.io/docs/2026-07-28/learn/server-concepts

## Achados

### Classificação da Pesquisa (routing)

- O workflow de routing classifica o input e despacha para um caminho especializado a jusante; a classificação pode ser feita por LLM ou por modelo/algoritmo tradicional — quem decide é configurável, não fixo no LLM [fonte: Anthropic Building Effective Agents, seção Routing].
- Routing serve para separar categorias tratadas melhor em separado e para rote ar perguntas fáceis/comuns a modelos menores e difíceis a modelos maiores (ex.: Haiku vs Sonnet) [fonte: Anthropic Building Effective Agents, seção Routing].
- No sistema de Research da Anthropic, o lead agent avalia a complexidade da query e dimensiona o esforço (nº de subagentes e tool calls) por regras explícitas no prompt, porque agentes não calibram esforço sozinhos [fonte: Anthropic Multi-agent research, item "Scale effort to query complexity"].
- Heurística de escala documentada: fact-finding simples = 1 agente com 3–10 tool calls; comparação direta = 2–4 subagentes com 10–15 calls cada; research complexa = 10+ subagentes com responsabilidades divididas [fonte: Anthropic Multi-agent research, item 3].

### Modo de Pesquisa (workflow vs agente autônomo)

- Workflows orquestram LLMs e tools por code paths pré-definidos; agentes deixam o LLM dirigir dinamicamente processos e tool usage [fonte: Anthropic Building Effective Agents, seção "What are agents?"].
- Agentes são indicados para problemas open-ended onde o nº de passos é imprevisível e não há como hardcodar o caminho; custam mais latência/custo e exigem guardrails e condição de parada (ex.: máximo de iterações) [fonte: Anthropic Building Effective Agents, seção Agents].
- Research é o caso canônico de agente: processo dinâmico e path-dependent, onde cada descoberta muda a abordagem — um pipeline linear one-shot não dá conta [fonte: Anthropic Multi-agent research, seção "Benefits of a multi-agent system"].
- Sistemas multi-agente vencem sobretudo queries breadth-first com direções independentes paralelizáveis; no eval interno, lead Opus 4 + subagentes Sonnet 4 superou o single-agent Opus 4 em 90,2% [fonte: Anthropic Multi-agent research, seção Benefits].
- 80% da variância de performance no eval BrowseComp é explicada por token usage; depois vêm nº de tool calls e escolha do modelo — arquitetura multi-agente escala tokens via context windows separadas [fonte: Anthropic Multi-agent research, seção Benefits].
- Custo observado: agentes usam ~4× tokens de um chat; sistemas multi-agente ~15× — só se pagam em tarefas de alto valor [fonte: Anthropic Multi-agent research, seção Benefits].

### Single-query vs multi-query (fan-out paralelo)

- Parallelization tem duas formas: sectioning (subtarefas independentes em paralelo) e voting (mesma tarefa N vezes, agrega) [fonte: Anthropic Building Effective Agents, seção Parallelization].
- Orchestrator-workers: um LLM central quebra a tarefa dinamicamente, delega a workers e sintetiza — subtarefas não são pré-definidas, e sim decididas pelo orquestrador por input [fonte: Anthropic Building Effective Agents, seção Orchestrator-workers].
- No Research: o lead agent decompõe a query e dispara 3–5 subagentes em paralelo; cada subagente usa 3+ tools em paralelo — isso cortou o tempo de research em até 90% [fonte: Anthropic Multi-agent research, item 8 "Parallel tool calling"].
- Estratégia de busca ensinada em prompt: começar wide com queries curtas e amplas, avaliar o que existe, depois estreitar progressivamente — queries longas e específicas prematuras retornam poucos resultados [fonte: Anthropic Multi-agent research, item 6 "Start wide, then narrow down"].
- Subagentes funcionam como compressão: cada um explora com seu próprio context window e devolve só os tokens condensados ao lead agent [fonte: Anthropic Multi-agent research, seção Benefits].
- Modo de Pesquisa p/ web-agent-research [INFERENCE a partir das fontes acima]: `single` = 1 Turno, sem fan-out (fact-finding); `paralelo` = fan-out determinístico N queries (comparação); `deep` = loop orchestrator-workers com re-planejamento por Turno.

### Classificar URLs (URL ranking)

- Server tool `web_search` retorna resultados com citações; com `web_search_20260209+` o Claude escreve código que filtra resultados antes do context window (dynamic filtering), reduzindo tokens em requests search-heavy [fonte: Anthropic Web search tool, seção Dynamic filtering].
- Dynamic filtering roda dentro de code execution provisionado pela API automaticamente, sem adicionar a tool manualmente e sem custo extra além de tokens [fonte: Anthropic Web search tool, seção Dynamic filtering].
- `max_uses` limita o nº de buscas por request e serve como restrição dura contra over-searching [fonte: Anthropic Web search tool, seção "How to use web search"].
- Busca é disparável por system prompt (incentivar buscar mais ou responder direto) e por domínios (`allowed_domains`/`blocked_domains`) [fonte: Anthropic Web search tool, seções "When Claude searches" e Console settings].
- Testers humanos do Research notaram viés por conteúdo SEO-farm acima de fontes autoritativas (PDFs acadêmicos, blogs pessoais); a correção foi heurística de qualidade de fonte no prompt [fonte: Anthropic Multi-agent research, seção "Human evaluation catches what automation misses"].
- Rubrica do LLM-judge do Research inclui source quality (primárias > secundárias de baixa qualidade) e tool efficiency (tools certas, nº razoável de calls) [fonte: Anthropic Multi-agent research, seção "LLM-as-judge evaluation"].
- Classificação de URLs p/ web-agent-research [INFERENCE]: ranking determinístico em código (primária > secundária, deduplicação por domínio/URL) + filtro LLM opcional; `max_uses` e allow/blocklist como parâmetros do Turno.

### Extrair (fetch + leitura)

- `web_fetch` é server tool: a API busca o conteúdo durante o request e injeta na conversa — o harness não executa nada nem devolve `tool_result` [fonte: Anthropic Web fetch tool, seção "How web fetch works"].
- Exceção: se no mesmo grupo de parallel calls houver client tool + `web_fetch`, a API retorna `stop_reason: tool_use` antes do fetch rodar; o fetch executa quando o harness devolve os `tool_result` dos client tools [fonte: Anthropic Web fetch tool, seção "How web fetch works"].
- `web_fetch` dispara quando há URL específica na conversa (ou recurso nomeado + `web_search` para localizá-la antes); não dispara para perguntas abertas de conhecimento geral [fonte: Anthropic Web fetch tool, seção "When Claude fetches"].
- `web_fetch_20260209+` suporta dynamic filtering (extrair só seções relevantes de docs longos/PDFs) sobre code execution automático [fonte: Anthropic Web fetch tool, seção "Dynamic filtering"].
- `web_fetch` atual não renderiza JS dinâmico; página que exige browser real (render, click, form) pede a client tool `browser use`, onde a aplicação pilota o browser e devolve texto/screenshot como tool result [fonte: Anthropic Web fetch tool, NOTA "does not support websites dynamically rendered with JavaScript"].
- Segurança: Claude só pode buscar URLs que já apareceram na conversa (mensagem do usuário, tool result client-side, resultados anteriores de search/fetch) — nunca URLs que existam só no próprio output; mesmo assim há risco residual de exfiltração [fonte: Anthropic Web fetch tool, Warning + seção "URL validation"].
- Mitigações documentadas: desabilitar fetch, `max_uses`, `allowed_domains` restrito a domínios seguros [fonte: Anthropic Web fetch tool, Warning].
- PDFs retornam como base64 e são processados como documento anexado [fonte: Anthropic Web fetch tool, seção "How web fetch works"].

### Interagir com sub-páginas (navegação multi-hop)

- O padrão agente é loop LLM→tool→ground truth do ambiente→re-planejar; a cada passo o agente ganha ground truth (tool call results, execução) para avaliar progresso [fonte: Anthropic Building Effective Agents, seção Agents].
- Subagentes do Research usam interleaved thinking após cada tool result para avaliar qualidade, achar gaps e refinar a próxima query [fonte: Anthropic Multi-agent research, item 7 "Guide the thinking process"].
- Lead agent usa extended thinking para planejar: quais tools cabem, complexidade da query, nº de subagentes, papel de cada um [fonte: Anthropic Multi-agent research, item 7].
- Delegação exige descrever a cada subagente: objetivo, formato de output, quais tools/fontes usar e fronteiras da tarefa; instrução curta e vaga ("research the semiconductor shortage") gerou duplicação e gaps [fonte: Anthropic Multi-agent research, item 2 "Teach the orchestrator how to delegate"].
- Confiabilidade em produção: sem mitigação, restart do zero é caro — o sistema retoma de onde parou (resume), com retry determinístico + checkpoints regulares, e avisa o modelo quando uma tool falha para ele se adaptar [fonte: Anthropic Multi-agent research, seção "Agents are stateful and errors compound"].
- Deploy sem quebrar agentes em voo usa rainbow deployments (versões velha e nova convivendo, migração gradual de tráfego) [fonte: Anthropic Multi-agent research, seção "Deployment needs careful coordination"].
- Limitação atual: execução síncrona lead→subagentes cria gargalo (lead não redireciona no meio, subagentes não coordenam entre si); execução assíncrona daria mais paralelismo ao custo de coordenação/consistência de estado [fonte: Anthropic Multi-agent research, seção "Synchronous execution creates bottlenecks"].

### Sintetizar (pequeno / médio / grande / deep)

- Prompt chaining decompõe a tarefa em passos sequenciais com gates programáticos de checagem entre eles; troca latência por acurácia tornando cada call LLM mais fácil [fonte: Anthropic Building Effective Agents, seção "Workflow: Prompt chaining"].
- Evaluator-optimizer: um LLM gera, outro avalia/critica em loop; indicado quando há critério claro de avaliação e refinamento iterativo agrega valor mensurável — análogo a rascunho→revisão humana [fonte: Anthropic Building Effective Agents, seção "Workflow: Evaluator-optimizer"].
- No Research, o lead sintetiza os achados dos subagentes e decide se mais research é necessária (novos subagentes ou refino de estratégia); ao sair do loop, um CitationAgent dedicado atribui cada claim à sua fonte [fonte: Anthropic Multi-agent research, diagrama "Process diagram" + legenda].
- Gerenciamento de contexto longo: agentes resumem fases concluídas e guardam o essencial em memória externa; perto do limite (ex.: truncamento acima de ~200k tokens) o plano é persistido e subagentes frescos são criados com handoff [fonte: Anthropic Multi-agent research, diagrama + Appendix "Long-horizon conversation management"].
- Para evitar o "telefone sem fio", subagentes gravam outputs (relatórios, código, visualizações) direto no filesystem/sistema externo e passam só referências leves ao coordenador [fonte: Anthropic Multi-agent research, Appendix "Subagent output to a filesystem"].
- Tamanhos de síntese p/ web-agent-research [INFERENCE calibrada nas fontes acima]: `pequeno` = resposta direta com citações (1 pass, sem loop evaluator); `médio` = prompt chaining resumo→resposta com 1 gate de completude; `grande` = orchestrator-workers + síntese do lead; `deep` = loop evaluator-optimizer com CitationAgent dedicado e persistência de Evidências.

### Seam Tool-calling e MCP como harness

- Tool calling OpenAI é conversa multi-passo em 5 etapas: request com tools → tool call do modelo → execução do código no lado da aplicação → segundo request com o output → resposta final (ou mais tool calls) [fonte: OpenAI Function calling, seção "The tool calling flow"].
- Function = tool definida por JSON schema; o modelo passa dados, a aplicação executa ação/acesso — execução sempre fora do modelo [fonte: OpenAI Function calling, seção "Functions versus tools"].
- Tool output pode ser JSON estruturado ou texto puro e referencia o `call_id` do tool call [fonte: OpenAI Function calling, seção "Tool call outputs"].
- Na Anthropic, client tools rodam na aplicação: resposta com `stop_reason: tool_use` + blocos `tool_use` (`id`, `name`, `input`); o harness executa e devolve `tool_result` numa mensagem `user` [fonte: Anthropic Tool use overview, seção "How tool use works"].
- Formato do `tool_result`: `tool_use_id` + `content` (string ou blocos `text`/`image`/`document`/`search_result`) + `is_error` opcional; blocos `tool_result` vêm PRIMEIRO no array, texto depois; nada entre o `tool_use` do assistant e a mensagem de resultados [fonte: Anthropic Handle tool calls].
- Erro de execução de tool: devolver mensagem instrutiva em `content` com `is_error: true` (ex.: "Rate limit exceeded. Retry after 60 seconds") em vez de "failed" genérico — o modelo se recupera sem adivinhar [fonte: Anthropic Handle tool calls, seção "Tool execution error"].
- Parâmetro inválido/ausente: devolver `tool_result` de erro; Claude retenta 2–3x com correções antes de desistir; `strict: true` elimina a classe inteira de erros de schema [fonte: Anthropic Handle tool calls, seção "Invalid tool name"].
- Conteúdo de tool result é não-confiável (páginas, e-mails, uploads, APIs): manter dentro de `tool_result`, nunca em system prompt ou texto `user` puro — defesa contra indirect prompt injection [fonte: Anthropic Handle tool calls, Warning].
- MCP é standard aberto: AI apps conectam a data sources, tools e workflows; "USB-C das AI applications" [fonte: MCP Intro].
- Arquitetura MCP: host (AI app) gerencia N clients, um por server, cada um com conexão dedicada; server local (STDIO, 1 client) vs remoto (Streamable HTTP, N clients) [fonte: MCP Architecture, seção Participants].
- Protocolo em 2 camadas: data layer (JSON-RPC 2.0: discovery, tools/resources/prompts, notifications, progress) e transport layer (STDIO p/ local, Streamable HTTP + auth/OAuth p/ remoto) [fonte: MCP Architecture, seção Layers].
- MCP é stateless: cada request carrega versão + capabilities em `_meta`; `server/discover` é obrigatório no server e opcional no client (pode chamar direto e tratar version error) [fonte: MCP Architecture, seção "Statelessness and discovery"].
- Primitivas do server: Tools (model-controlled: o LLM descobre via `tools/list` e invoca via `tools/call`, JSON Schema), Resources (application-driven: URIs + MIME, diretas ou templates), Prompts (user-controlled: templates invocados explicitamente) [fonte: MCP Server concepts + MCP Architecture, seção Primitives].
- Elicitation (`elicitation/create` via multi round-trip) permite ao server pedir input/confirmação ao usuário; sampling está deprecated desde `2026-07-28` — novas implementações integram direto com LLM provider APIs [fonte: MCP Architecture, seção Primitives].
- OpenAI expõe MCP remoto via tool tipo `mcp` na Responses API com `server_label`, `server_url` (ou `connector_id` OpenAI-managed) e `require_approval` (`never`/condicional) [fonte: OpenAI MCP and Connectors, seção Quickstart].
- Aviso OpenAI: confiar num MCP remoto é crítico — server malicioso pode exfiltrar qualquer dado que entre no contexto do modelo [fonte: OpenAI MCP and Connectors, callout "It is very important that developers trust any remote MCP server"].
- Doc de tool importa tanto quanto prompt: descrição com propósito distinto, exemplo de uso, edge cases, formato de input e fronteiras com outras tools; testar dezenas de vezes e reescrever (um caso deu −40% no tempo de conclusão) [fonte: Anthropic Multi-agent research, itens 4–5; Building Effective Agents, Appendix 2].
- Poka-yoke nas tools: mudar argumentos para tornar o erro difícil (ex.: exigir absolute paths em vez de relative) [fonte: Anthropic Building Effective Agents, Appendix 2].

### Estado: Sessão e Turno

- O loop OpenAI é stateful no input acumulado: a app mantém a running input list (prompt + tool definitions + tool calls + outputs) e reenvia a cada request (ou usa `PreviousResponseID`) [fonte: OpenAI Function calling, exemplo `get_horoscope`].
- Na Anthropic a conversa é arrays de blocos `text`/`image`/`tool_use`/`tool_result` em mensagens `user`/`assistant` — sem role `tool`/`function` separada [fonte: Anthropic Handle tool calls, Tip "Differences from other APIs"].
- MCP servers são stateless por protocolo; estado de longa duração vive no host/app (memória externa, checkpoints), não no server [fonte: MCP Architecture, seção "Statelessness and discovery"].
- Research persiste plano em Memory para sobreviver a truncamento de contexto e retoma agentes do checkpoint em vez de reiniciar [fonte: Anthropic Multi-agent research, diagrama + seção "Agents are stateful"].
- Mapeamento p/ web-agent-research [INFERENCE]: Sessão = input list acumulada + store de Evidências com URL+timestamp; Turno = uma volta planejar→buscar→ler→sintetizar com `PreviousResponseID`/cursor próprio; truncamento/compacção por resumo de fase.

### Erro e retry por estágio

- Classificação: erro aqui desvia o pipeline inteiro — mitigar com fallback determinístico (default `single`) e não só retry LLM [INFERENCE a partir de: routing misclassification degrada todas as categorias, Building Effective Agents, seção Routing].
- Busca: `max_uses_exceeded`, `too_many_requests`, `query_too_long`, `invalid_input`, `unavailable` são os códigos de erro do server tool web_search [fonte: Anthropic Handle tool calls, seção "Server tool errors"].
- Fetch: mesmas classes de erro de server tool; conteúdo JS-heavy falha silenciosamente em fetch estático → fallback é trocar de tool (browser use), não retentar [fonte: Anthropic Web fetch tool, NOTA JS + Handle tool calls, "Server tool errors"].
- Agente como um todo: combinar adaptabilidade do modelo (avisar da falha e deixar adaptar) com salvaguardas determinísticas (retry logic, checkpoints regulares) [fonte: Anthropic Multi-agent research, seção "Agents are stateful and errors compound"].
- Observabilidade: tracing completo de produção + monitoramento de padrões de decisão/estruturas de interação (sem conteúdo) para diagnosticar "não achou o óbvio" sistematicamente [fonte: Anthropic Multi-agent research, seção "Debugging benefits from new approaches"].

## Implicação p/ web-agent-research

- Adotar taxonomia routing→modo: classificador (LLM ou regra determinística) escolhe entre `single`, `paralelo` e `deep` com budget explícito de subagentes/tool calls por complexidade (1/3–10; 2–4/10–15 cada; 10+), com fallback determinístico para `single`.
- Implementar o Turno como loop agente canônico (LLM decide próxima tool; código executa; ground truth volta como `tool_result` JSON referenciado por `tool_use_id`), com condição de parada (máx. Turnos/tool calls) e `max_uses` por request.
- Expor o pipeline ao harness em dois seams complementares: (a) function/tool-calling nativo do gateway OpenAI-compatible para CLI→API de baixa fricção; (b) MCP server (`tools/list` + `tools/call`, STDIO local primeiro, Streamable HTTP depois) para hosts terceiros — Tools model-controlled, Resources (Evidências, plano da Sessão) application-driven, Prompts (templates pequeno/médio/grande/deep) user-controlled.
- Definir tools do pipeline com interface ACI tratada como HCI: `pesquisar` (multi-query fan-out), `ranquear_urls` (ranking determinístico + filtro LLM + allow/blocklist), `extrair` (fetch estático; fallback browser p/ JS), `sintetizar` (4 tamanhos com gates: direto / chaining+gate / workers+lead / evaluator+CitationAgent) — cada uma com descrição distinta, schema strict, exemplos e edge cases.
- Estado: Sessão persiste input acumulado + Evidências (URL fonte + momento da coleta); Turno persiste cursor/plano para resume de checkpoint; compacção por resumo de fase + spill de outputs de subagente para store externo com referência leve (evita "telefone sem fio" e estouro de contexto).
- Consumo pelo harness: `tool_result` JSON estruturado legível por máquina (estágio, Evidências, citações, próximo passo) para a API; stdout humano-legível (resumo + citações) para o CLI — conteúdo web sempre dentro de `tool_result`, nunca em system prompt.
- Erros por estágio com códigos próprios espelhando os server tools (`too_many_requests`, `max_uses_exceeded`, `query_too_long`, `unavailable`) + `is_error` instrutivo com ação de recovery; troca de tool (fetch→browser) preferida a retry cego; tracing de decisões por Turno desde o dia 1.
