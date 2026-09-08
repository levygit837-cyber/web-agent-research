# Main só via PR, sem plano pago

Repo privado no plano free não tem branch protection nem rulesets (a API retorna 403, verificado em 2026-09-07); decidimos convenção PR-only para a `main` com detecção pós-push (workflow `protect-main`) + bloqueio local (hook `pre-push` em `.githooks/`), aceitando que o push direto não pode ser bloqueado no servidor até migrar para Pro ou repo público.

## Considered Options

- Tornar o repo público — libera protection/rulesets de graça; custo é expor protótipo e histórico antes da hora.
- Assinar GitHub Pro — libera protection no privado; custo é mensalidade para um protótipo solo.
- Só convenção sem enforcement — zero código; custo é depender de disciplina, sem sinal quando falhar.
- Pre-receive hooks no servidor — bloqueio real customizado; custo é exigir GitHub Enterprise, fora do alcance.

## Consequences

- Toda mudança na `main` entra por PR com merge commit (`gh pr create` → `gh pr merge`); push direto é violação, mesmo com CI vermelho — emergência se resolve com PR de revert, nunca com push.
- `protect-main.yml` (push na `main`) falha quando algum commit novo não é merge de PR com estado MERGED nem tem PR mergeado associado via API de commits; a falha traz o passo a passo (revert em branch nova + PR, sem force-push — reescrever a `main` também dispara o gate).
- Hook `.githooks/pre-push` bloqueia `push` local a `refs/heads/main`; ativar por clone com `git config core.hooksPath .githooks` (hook é opt-in por clone, não versionável como obrigatório).
- CI (`ci.yml`) roda `lint` (fmt + clippy) e `test` (test + build `--locked`) em push/PR na `main`; na migração, `lint` + `test` + `no-direct-push` viram os checks obrigatórios, com PR exigido, 1 approval, `dismiss stale approvals`, branch atualizada, sem force push nem delete.
- Sem isenções: recriar a `main` via push falha fechado — até o commit raiz exige PR associado (o bootstrap histórico é anterior ao gate).
