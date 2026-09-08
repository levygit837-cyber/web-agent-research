# Web Agent Research

Protótipo CLI de agente multi-turno para pesquisa na web sobre a engine Obscura, que evolui para API.

## Language

**Pesquisa**:
Objetivo de pesquisa do usuário executado pelo agente até síntese final.
_Avoid_: query, tarefa, job

**Sessão**:
Execução multi-turno de uma Pesquisa, com histórico recuperável de turnos.
_Avoid_: conversa, thread, run

**Turno**:
Uma iteração do loop planejar → buscar → ler → sintetizar dentro de uma Sessão.
_Avoid_: passo, iteração, ciclo

**Evidência**:
Conteúdo extraído da web com URL fonte e momento da coleta, usado na síntese.
_Avoid_: fonte, documento, snippet, chunk

**Mode**:
Execution shape of a Research: `search` (agent loop over search → fetch → synthesize) or `deep` (same loop with page interaction and re-planning).
_Avoid_: workflow, strategy

**Synthesis**:
Final explanatory summary of a Research's Evidence, sized `small`, `medium`, `large` or `deep`.
_Avoid_: report
