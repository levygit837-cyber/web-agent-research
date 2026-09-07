# Embeddings, RAG e Memória em Sistemas de Agentes

> Research note do protótipo CLI de agente multi-turno (loop planejar → buscar → ler → sintetizar).
> Público: iniciante leigo. Prosa em pt-BR; termos técnicos mantidos em inglês (`embedding`, `chunking`, `vector store`, `retrieval`, `rerank`, etc.).
> Escopo: entender o estado atual de RAG/embeddings e avaliar, com honestidade, o que faria (ou não) sentido no sistema atual de web-search com Agentes Assíncronos via Obscura Engine Browser.

**Como ler este documento:** a seção 0 dá a visão de 5 minutos. Seções 1–2 são a base para leigos. Seção 3 é o catálogo de tecnologias. Seção 4 explica memória em agentes. Seção 5 traz cenários práticos em Agentes. Seção 6 é a única que fala deste repo — pode ser lida de forma isolada por quem já conhece o básico.

---

## 0. Visão geral em 5 minutos

- **Embedding** é transformar um texto em uma lista de números (um vetor) que captura o *significado* do texto. Textos com significado parecido viram vetores "próximos" entre si. É assim que o computador passa a comparar ideias, não só letras ([docs OpenAI embeddings](https://developers.openai.com/api/docs/guides/embeddings), [intro Cohere embeddings](https://docs.cohere.com/docs/embeddings)).
- **RAG (Retrieval-Augmented Generation)** é um padrão em 2 fases: (1) **retrieval** — buscar os trechos mais relevantes para a pergunta; (2) **generation grounded** — pedir ao LLM para responder *usando aqueles trechos como contexto*, citando fontes. Isso reduz alucinação sobre dados que o modelo não viu no treino ([intro RAG LlamaIndex](https://developers.llamaindex.ai/python/framework/understanding/rag/)).
- Um sistema RAG típico faz: quebrar documentos em pedaços (**chunking**) → gerar embeddings → guardar num **vector store** → diante de uma pergunta, buscar os K pedaços mais similares → opcionalmente reordenar com um **reranker** → montar o prompt com os trechos + pergunta → gerar a resposta.
- **Para que serve:** busca semântica ("mudança climática" acha "aquecimento global"), Q&A sobre base própria, recomendação, dedup/clustering, memória de agente ([usos listados pela OpenAI](https://developers.openai.com/api/docs/guides/embeddings#what-are-embeddings)).
- **Quando NÃO usar:** se a resposta já está no prompt, se o dado muda a cada segundo, se a pergunta exige palavra exata (código de erro, SKU), se o corpus é minúsculo (dezenas de itens cabem no prompt), ou se você precisa de raciocínio multi-hop com garantias — aí busca híbrida, rerank ou até SQL/full-text resolvem melhor e mais barato (detalhes na seção 2.5).
- **Peças complementares que importam mais que o modelo de embedding:** busca híbrida (**BM25 + vetor**), **rerankers** (ex.: [Cohere Rerank](https://docs.cohere.com/v2/docs/rerank)), **ColBERT late-interaction** ([repo oficial](https://github.com/stanford-futuredata/ColBERT)), vetores **sparse** (SPLADE), e **Knowledge Graphs / GraphRAG** ([repo Microsoft](https://github.com/microsoft/graphrag)) para perguntas que atravessam várias entidades.
- **Em Agentes**, embeddings viram *memória*: memória vetorial de longo prazo (ex.: [Letta, ex-MemGPT](https://github.com/cpacker/MemGPT)), `store` com busca vetorial no [LangChain/LangGraph](https://docs.langchain.com/oss/python/langchain/long-term-memory), índices e `retrievers` no [LlamaIndex](https://developers.llamaindex.ai/python/framework/understanding/rag/), cache semântico de respostas e RAG agentico/hierárquico.
- **Neste repo** (CLI file-based, sem DB por [ADR-0002](../../adr/0002-sem-db-so-arquivos.md), vocabulário Pesquisa/Sessão/Turno/Evidência): os ganhos realistas de curto prazo são **dedup semântica de Evidências entre Turnos**, **rerank de Evidências antes da síntese** e **cache semântico de Pesquisas passadas** — todos implementáveis sem virar o projeto de cabeça para baixo (detalhes e matriz esforço/valor na seção 7). Um vector store pesado (Qdrant, Milvus, Weaviate gerenciado) ou GraphRAG completo seria over-engineering hoje; o caminho compatível com "sem DB" seria no máximo `sqlite-vec` local ([repo sqlite-vec](https://github.com/asg017/sqlite-vec)).

---

## 1. Conceitos para iniciante leigo

### 1.1 A analogia central: mapa de significados

Imagine cada texto como uma casa num mapa. O embedding é o **endereço numérico** dessa casa. Textos sobre o mesmo assunto moram no mesmo bairro; assuntos diferentes moram longe.

```
"amo sopa"                  -> [0.21, 0.87, -0.12, ...]   (bairro: comida)
"sopa é minha favorita"     -> [0.23, 0.85, -0.10, ...]   (mesmo bairro!)
"Londres é longe"           -> [-0.71, 0.05, 0.66, ...]   (outro bairro)
```

Exemplo real desse raciocínio (comparar "i love soup" × "soup is my favorite" × "london is far away") está no [guia da Cohere](https://docs.cohere.com/docs/embeddings). A "distância no mapa" é a medida de similaridade — ver 1.3.

Por que isso é útil? Busca por palavra-chave só acha o que **contém as letras**. Busca por embedding acha o que **quer dizer a mesma coisa**. O [overview do Qdrant](https://qdrant.tech/documentation/overview/) usa exatamente este exemplo: buscar "climate change" recupera documentos sobre "global warming" mesmo sem palavra em comum.

### 1.2 O que é um embedding, de verdade

Um embedding é um **vetor de floats** (lista de números decimais) de tamanho fixo. Tamanhos típicos: 384 (modelos pequenos locais), 768/1024 (modelos médios/multilíngues), 1536 (`text-embedding-3-small`) ou 3072 (`text-embedding-3-large`) ([docs OpenAI](https://developers.openai.com/api/docs/guides/embeddings#embedding-models)).

```text
"Obscura é uma engine de browser"  ->  [-0.012, 0.044, -0.31, ..., 0.007]  (1536 números)
```

Três propriedades que o iniciante precisa internalizar:

1. **Dimensão fixa, significado distribuído.** Nenhuma posição isolada "significa" algo; o sentido está no padrão do vetor inteiro.
2. **Modelos diferentes = mapas diferentes.** Vetores do OpenAI não são comparáveis com vetores do BGE ou do E5. Escolheu o modelo, use o mesmo para indexar e para consultar.
3. **Janela de entrada limitada.** Cada modelo aceita no máximo N tokens por texto (ex.: 8192 no `text-embedding-3-*` ([OpenAI](https://developers.openai.com/api/docs/guides/embeddings#embedding-models)), 32000 na família Voyage-4 ([Voyage docs](https://docs.voyageai.com/docs/embeddings#model-choices))). Texto maior que isso precisa de chunking (seção 1.4) ou será truncado.

Exemplo mínimo (Python, API OpenAI — [referência do endpoint](https://developers.openai.com/api/docs/guides/embeddings#how-to-get-embeddings)):

```python
from openai import OpenAI
client = OpenAI()

resp = client.embeddings.create(
    input="Obscura é uma engine de browser para agentes",
    model="text-embedding-3-small",
)
vetor = resp.data[0].embedding  # lista de 1536 floats
print(len(vetor), vetor[:3])
```

### 1.3 Similaridade: a régua do mapa

Dado o vetor da pergunta (query) e os vetores dos documentos, calculamos uma **distância**. Quanto menor a distância (ou maior a similaridade), mais relevante.

| Medida | Intuição | Quando aparece |
|---|---|---|
| **Cosine similarity** | ângulo entre vetores; ignora magnitude | padrão em busca de texto; com vetores normalizados (norma L2 = 1), cosine equivale a dot product ([Jina: normalização L2](https://jina.ai/embeddings/)) |
| **Dot product** | produto interno; rápido se vetores normalizados equivale ao cosseno | default em vários stores |
| **Euclidean (L2)** | distância "em linha reta" no mapa | `pgvector` usa `<->` para L2 ([pgvector README](https://github.com/pgvector/pgvector)) |
| **Inner product / maxSim** | produto interno (ou MaxSim por token no ColBERT) | `pgvector` usa `<#>`; ColBERT usa MaxSim token-a-token ([ColBERT README](https://github.com/stanford-futuredata/ColBERT)) |

Na prática: **se normalizar os vetores (norma L2 = 1), cosine, dot e L2 ordenam igual**. Por isso APIs como Jina expõem flag `normalized: true` ([Jina Embeddings](https://jina.ai/embeddings/)). Não gaste energia escolhendo a métrica no início — gaste escolhendo bons chunks e avaliando o top-K.

Exemplo mínimo de busca (sem servidor nenhum, só NumPy):

```python
import numpy as np

def cosine(a, b):
    a, b = np.array(a), np.array(b)
    return float(np.dot(a, b) / (np.linalg.norm(a) * np.linalg.norm(b)))

q = client.embeddings.create(input="engine de browser para agentes", model="text-embedding-3-small").data[0].embedding
docs = ["Receita de bolo de cenoura", "Obscura Engine: browser headless para automação", "Tabela do campeonato"]
scores = [(d, cosine(q, client.embeddings.create(input=d, model="text-embedding-3-small").data[0].embedding)) for d in docs]
print(sorted(scores, key=lambda x: -x[1]))
# esperado: o doc da Obscura pontua mais alto, mesmo sem compartilhar todas as palavras
```

### 1.4 Chunking: por que quebrar textos (e como não quebrar errado)

Modelos têm limite de tokens e, pior, **vetores de textos longos "diluem" o sentido** (um manual de 50 páginas vira um ponto médio sem graça no mapa). Por isso dividimos documentos em **chunks** (pedaços) antes de vetorizar. No LlamaIndex, o chunk é a unidade atômica chamada `Node` ([docs LlamaIndex](https://developers.llamaindex.ai/python/framework/understanding/rag/#important-concepts-within-rag)).

Regras práticas de iniciante:

1. **Respeite fronteiras naturais.** Quebre por parágrafo/seção/markdown heading, nunca no meio de uma frase. Para HTML da web, extraia o texto principal antes (readability) e descarte menu/rodapé.
2. **Tamanho típico: 200–800 tokens com overlap de 10–20%.** Overlap (repetir o final do chunk anterior no início do próximo) evita que uma ideia cortada na borda se perca.
3. **Um chunk = uma ideia.** Se o chunk mistura 5 assuntos, o vetor vira "sopa". Prefira chunks menores e deixe o reranker juntar.
4. **Guarde metadados junto do vetor:** URL fonte, `fetched_at`, título, posição do chunk. Sem isso você não consegue citar fonte nem invalidar cache (isso será crucial na seção 7).

```
Documento (página web sobre Obscura)
  ├── chunk 1: "Obscura Engine é..." (tokens 0–400)      + meta {url, fetched_at}
  ├── chunk 2: "...suporta CDP..." (tokens 350–750)      + meta {url, fetched_at}  <- overlap
  └── chunk 3: "...modo headless..." (tokens 700–1000)   + meta {url, fetched_at}
```

Anti-padrões comuns: chunks gigantes ("cada página é um vetor"), chunks de tamanho fixo em caracteres cortando palavra, e jogar fora a URL após vetorizar (você perde a citação — o "grounded" do RAG morre aqui).

### 1.5 Retrieval, rerank e geração grounded (o trio do RAG)

- **Retrieval (recuperação):** diante da pergunta, trazer os K candidatos mais similares. É recall alto, precisão média — traz 20–50 candidatos rápido via índice aproximado (HNSW e similares; o [overview do Qdrant](https://qdrant.tech/documentation/overview/#data-structure) e o [pgvector](https://github.com/pgvector/pgvector) documentam HNSW).
- **Rerank (reordenação):** um segundo modelo, mais caro e mais preciso, relê a pergunta + cada candidato e dá nota fina. Ficamos com os top 3–8. É o maior ganho qualidade/custo na maioria dos RAGs (ver [Cohere Rerank](https://docs.cohere.com/v2/docs/rerank)).
- **Geração grounded:** o LLM responde **só com base nos trechos**, citando quais usou. "Grounded" = ancorado em fontes, não na memória do modelo. Se nenhum trecho presta, a resposta correta é "não encontrei nas fontes" — não inventar.

```
pergunta ──► retrieval (barato, top-50) ──► rerank (caro, top-5) ──► prompt(trechos + pergunta) ──► resposta com citações
```

---

## 2. RAG passo a passo

Visão canônica em 5 estágios (nomenclatura do [LlamaIndex](https://developers.llamaindex.ai/python/framework/understanding/rag/#stages-within-rag)): **Loading → Indexing → Storing → Querying → Evaluation**.

### 2.1 Loading (carregar)

Tirar o dado de onde ele vive (PDF, site, API, DB) e trazer para o pipeline. No nosso caso: páginas buscadas via Obscura Engine Browser. Ponto de atenção: **extração de texto principal** (boilerplate de site polui embedding) e normalização de URL (para dedup e cache — eco direto do [ADR-0002](../../adr/0002-sem-db-so-arquivos.md)).

### 2.2 Indexing (indexar: chunk + embed)

```python
# Indexar (pseudo-Python mínimo)
from openai import OpenAI
client = OpenAI()

def chunk(text, size=500, overlap=50):
    toks = text.split()  # simplificação; na prática use tokenizer do modelo
    return [" ".join(toks[i:i+size]) for i in range(0, len(toks), size-overlap)]

chunks = chunk(pagina_texto)
vecs = client.embeddings.create(input=chunks, model="text-embedding-3-small").data
# guardar: (chunk_texto, vetor, {url, fetched_at})  -> vector store
```

Na prática de produção usa-se batch (a Voyage aceita até 1000 textos por chamada com teto de tokens ([Voyage docs](https://docs.voyageai.com/docs/embeddings#python-api))), `input_type="document"` vs `"query"` quando o modelo distingue (Cohere e Voyage recomendam marcar query vs documento ([Cohere input_type](https://docs.cohere.com/docs/embeddings#the-inputtype-parameter), [Voyage input_type](https://docs.voyageai.com/docs/embeddings#python-api))), e truncamento explícito.

### 2.3 Storing (guardar)

Guardar vetores + metadados num **vector store** para não reindexar tudo a cada pergunta ([LlamaIndex: storing](https://developers.llamaindex.ai/python/framework/understanding/rag/#stages-within-rag)). Opções na seção 3.2 — do `sqlite-vec` local (um arquivo, sem servidor) ao Pinecone gerenciado.

Exemplo com `sqlite-vec` (extensão SQLite em C puro, tabelas `vec0`, KNN por `ORDER BY distance` — [repo](https://github.com/asg017/sqlite-vec)):

```sql
.load ./vec0
CREATE VIRTUAL TABLE evidencias USING vec0(embedding FLOAT[1536], url TEXT, coletada_em TEXT);
INSERT INTO evidencias(rowid, embedding, url, coletada_em) VALUES (1, '[...]', 'https://...', '2026-09-07');
SELECT rowid, distance FROM evidencias
 WHERE embedding MATCH '[...vetor da pergunta...]'
 ORDER BY distance LIMIT 5;
```

### 2.4 Querying (consultar: retrieve → rerank → sintetizar)

```python
# Buscar (pseudo-Python mínimo)
qvec = client.embeddings.create(input="O que é Obscura Engine?", model="text-embedding-3-small").data[0].embedding
candidatos = store.search(qvec, top_k=30)          # retrieval: rápido, recall alto
top = rerank("O que é Obscura Engine?", candidatos, top_k=5)  # rerank: lento, preciso
resposta = llm(f"Responda usando SÓ estes trechos + cite URLs:\n{top}\n\nPergunta: ...")
```

Variações reais de querying: sub-queries (quebrar a pergunta), multi-step (buscar → ler → buscar de novo — exatamente o loop deste repo), e estratégias híbridas ([LlamaIndex: querying/retrievers/routers/postprocessors](https://developers.llamaindex.ai/python/framework/understanding/rag/#querying-stage)).

### 2.5 Quando NÃO usar RAG (e o que usar no lugar)

| Situação | Por que RAG é ruim aqui | Melhor alternativa |
|---|---|---|
| Resposta cabe no prompt / dado já coletado | retrieval adiciona latência, custo e ruído | passe os trechos direto no contexto |
| Dado muda a cada segundo (preço, status) | índice nasce desatualizado | chamada de ferramenta/API na hora (function calling), não índice |
| Busca por código exato (erro `E_CONN`, SKU, CPF) | embedding ignora string exata; "parecido" ≠ "igual" | BM25/keyword ou SQL `WHERE` (híbrido com `alpha=0` é busca pura por keyword — [Weaviate hybrid alpha](https://docs.weaviate.io/weaviate/search/hybrid)) |
| Corpus minúsculo (< 50 docs) | infra de vetor não se paga | `grep`/full-text ou tudo no prompt |
| Pergunta multi-hop com garantias ("quem é o CTO da empresa que comprou X em 2023?") | top-K por similaridade raramente monta a cadeia | GraphRAG / Knowledge Graph ([repo](https://github.com/microsoft/graphrag)) ou agente que decompõe em sub-buscas |
| Precisão jurídica/médica com citação auditável | LLM pode parafrasear errado mesmo grounded | retrieval + resposta extrativa (citar span literal) + revisão humana |

Regra de bolso: **RAG é para "relevância aproximada sobre corpus estável".** Fora disso, é custo sem benefício.

### 2.6 Avaliação (a etapa que todo iniciante pula)

Um RAG sem métrica é superstição. O LlamaIndex lista evaluation como estágio de primeira classe ([LlamaIndex](https://developers.llamaindex.ai/python/framework/understanding/rag/#stages-within-rag)). Mínimo viável: monte 20–30 perguntas com resposta esperada + URLs que *deveriam* ser recuperadas, e meça **recall@K** (a fonte certa veio nos K?) e **fidelidade** (a resposta cita e não inventa?). Rode esse conjunto a cada mudança de chunking/modelo/rerank. Sem isso, qualquer "melhoria" é achismo.

---

## 3. Tecnologias: modelos, stores, rerank, híbrido, grafos

### 3.1 Modelos de embedding

| Modelo | Tipo / acesso | Dimensões / contexto | Destaque | Fonte |
|---|---|---|---|---|
| `text-embedding-3-small` / `-large` | API paga OpenAI | 1536 / 3072; 8192 tokens; `dimensions` configurável (Matryoshka) | custo-benefício; small faz ~62500 páginas/dólar | [docs OpenAI](https://developers.openai.com/api/docs/guides/embeddings) |
| `text-embedding-ada-002` (legado) | API OpenAI | 1536; 8192 | anterior; trocar por v3 | [docs OpenAI](https://developers.openai.com/api/docs/guides/embeddings#embedding-models) |
| Cohere `embed-v4.0` / `embed-multilingual-v3.0` | API paga | até 100+ idiomas; `input_type` (query/document/classification); compressão `int8/binary` | multilíngue + imagem/documento; `output_dimension` 256–1536 | [Cohere embeddings](https://docs.cohere.com/docs/embeddings) |
| Voyage `voyage-4-large/4/4-lite`, `voyage-code-4`, `voyage-finance-2`, `voyage-law-2` | API paga | 32k tokens; dims até 2048; `input_type` query/document | modelos por domínio (código, finanças, jurídico); quantização `int8/binary` | [Voyage docs](https://docs.voyageai.com/docs/embeddings) |
| Jina `v5-text / v5-omni` | API + pesos abertos | 32k contexto; Matryoshka; GGUF/MLX p/ edge | multilíngue, multimodal (texto+imagem+áudio+vídeo no mesmo espaço) | [Jina embeddings](https://jina.ai/embeddings/) |
| BGE (`BAAI/bge-small-en-v1.5`, `bge-m3`, `bge-reranker`) | open-source (MIT) | small=384 dims, leve p/ CPU | rei do custo zero; `bge-m3` faz denso+sparse junto | [model card BGE-small](https://huggingface.co/BAAI/bge-small-en-v1.5) |
| E5 (`intfloat/multilingual-e5-large`) | open-source (MIT) | multilíngue amplo | forte em MTEB multilíngue; bom default local | [model card E5](https://huggingface.co/intfloat/multilingual-e5-large) |

Como escolher (ordem prática): **(1)** precisa de pt-BR bom? prefira multilíngue (E5, BGE-M3, Cohere, Voyage); **(2)** pode chamar API externa? OpenAI/Voyage/Cohere vencem em qualidade, BGE/E5 vencem em custo/privacidade; **(3)** compare no **MTEB** (benchmark público de embeddings citado pela [OpenAI](https://developers.openai.com/api/docs/guides/embeddings#embedding-models) e pela [Voyage](https://docs.voyageai.com/docs/embeddings)), mas valide no *seu* corpus (seção 2.6) — ranking de benchmark nem sempre transfere.

Notas que economizam dinheiro: reduza dimensão via Matryoshka (`output_dimension`/`dimensions`) quando guardar milhões de vetores ([Cohere Matryoshka](https://docs.cohere.com/docs/embeddings#matryoshka-embeddings), [Voyage output_dimension](https://docs.voyageai.com/docs/embeddings#python-api)); use `int8`/binário para cache grande ([Cohere compression](https://docs.cohere.com/docs/embeddings#compression-levels), [Voyage quantization](https://docs.voyageai.com/docs/embeddings#python-api)).

Fundamento acadêmico: o retrieval denso moderno nasce do **DPR** (Dense Passage Retrieval, Karpukhin et al. 2020 — [arxiv 2004.04906](https://arxiv.org/abs/2004.04906)): dois encoders (pergunta e passagem) treinados para aproximar pares relevantes. Tudo nesta tabela é descendente dessa ideia.

### 3.2 Vector stores (onde moram os vetores)

| Store | Modelo operacional | Quando preferir | Fonte |
|---|---|---|---|
| **sqlite-vec / sqlite-vss** | extensão SQLite, 1 arquivo, sem servidor, roda em qualquer lugar (Rust via `cargo add sqlite-vec`) | **protótipos file-based como este repo**; milhares–centenas de milhares de vetores; zero infra | [sqlite-vec](https://github.com/asg017/sqlite-vec) |
| **pgvector** | extensão Postgres; SQL com `<->` (L2), `<=>` (cosseno), `<#>` (inner); HNSW/IVFFlat; denso+sparse+binário | você já tem Postgres e quer vetor junto do relacional (JOIN, ACID) | [pgvector](https://github.com/pgvector/pgvector) |
| **Qdrant** | OSS em Rust + Cloud; coleções/payload/filtros; denso+sparse; híbrido; HNSW | controle fino + filtros de metadados em uma passada; bom self-host | [overview Qdrant](https://qdrant.tech/documentation/overview/) |
| **Weaviate** | OSS + Cloud; híbrido BM25F+vetor com `alpha`; módulos de vetorização | quer híbrido "de fábrica" e esquemas com texto embutido | [Weaviate hybrid](https://docs.weaviate.io/weaviate/search/hybrid) |
| **Milvus** | distribuído, escala bilionária | escala massiva; overkill abaixo de milhões | [docs Milvus](https://milvus.io/docs) |
| **Chroma** | OSS leve (Python/JS), embutível ou Cloud; denso+sparse+híbrido, filtros, multimodal | RAG local rápido em Python sem operar nada | [Chroma intro](https://docs.trychroma.com/docs/overview/introduction) |
| **Pinecone** | serverless gerenciado | time que não quer operar infra e paga por isso | [Pinecone docs](https://docs.pinecone.io/guides/get-started/overview) |

Leitura para este repo (eco do [ADR-0002](../../adr/0002-sem-db-so-arquivos.md), "sem DB, só arquivos"): a única opção que **não viola a decisão** é `sqlite-vec` — continua sendo "um arquivo", sem docker, sem serviço. Todo o resto (pgvector, Qdrant, Weaviate, Milvus, Pinecone) exige servidor/container/conta — ou seja, exige revisitar o ADR. Ver seção 7.

### 3.3 Além do vetor denso: híbrido, sparse, ColBERT, rerank

Vetor denso erra em **termo raro/exato** ("Obscura Engine v2.4.1", `ERR_TIMEOUT_CDP`). A cura é combinar sinais:

- **BM25 (keyword clássico):** conta frequência do termo com saturação + normaliza por tamanho do doc. Continua imbatível para match exato. Weaviate combina BM25F + vetor com parâmetro `alpha` (1 = só vetor, 0 = só keyword) ([Weaviate hybrid](https://docs.weaviate.io/weaviate/search/hybrid)).
- **Vetores sparse (SPLADE):** vetor gigante e quase todo zero, com peso por termo do vocabulário — "BM25 aprendido". Qdrant e Chroma suportam sparse ao lado do denso; o BGE-M3 gera os dois juntos ([Qdrant overview: sparse](https://qdrant.tech/documentation/overview/#retrieval-process), [Chroma: sparse+hybrid](https://docs.trychroma.com/docs/overview/introduction), [BGE](https://huggingface.co/BAAI/bge-small-en-v1.5)).
- **Busca híbrida = denso + sparse/keyword com fusão** (RRF ou soma ponderada). Regra: comece híbrido quando seus usuários buscam nomes próprios, versões, erros ou SKUs.
- **Rerankers:** modelo cross-encoder que lê (query, documento) juntos e dá nota. [Cohere Rerank v4/v3.5](https://docs.cohere.com/v2/docs/rerank) (multilíngue, 4096 tokens, trata JSON semi-estruturado) e `bge-reranker` open-source (família [BGE](https://huggingface.co/BAAI/bge-small-en-v1.5)). Padrão: retrieve 30–100 → rerank top 5–10. É onde mora o maior salto de qualidade por real gasto.
- **ColBERT late-interaction:** em vez de 1 vetor por texto, guarda **1 vetor por token** e pontua com **MaxSim** (cada token da query casa com o token mais parecido do doc). Mais preciso que vetor único, mais pesado que ele, mais barato que rerank em tudo ([repo + papers](https://github.com/stanford-futuredata/ColBERT)). Quando usar: corpus técnico onde detalhe por token importa e você pode pagar o índice maior.

```
query "obscura cdp headless"          doc "...Obscura via CDP em modo headless..."
  token "cdp" ──MaxSim──► "CDP" (0.97)     ← match por token, não por média do doc
  token "headless" ──► "headless" (0.99)
```

### 3.4 Knowledge Graphs e GraphRAG

Quando a pergunta exige **conectar fatos espalhados** ("quais ferramentas citadas pelas Evidências X e Y compartilham o mesmo mantenedor?"), similaridade por vetor falha: nenhum chunk isolado contém a resposta. A saída é um **Knowledge Graph**: extrair entidades e relações do texto (com LLM) e responder atravessando o grafo.

O **GraphRAG** da Microsoft é a referência aberta: pipeline que extrai grafo do texto não-estruturado, detecta comunidades e gera respostas globais sobre o dataset ([repo](https://github.com/microsoft/graphrag), [paper arxiv 2404.16130](https://arxiv.org/pdf/2404.16130)). Avisos honestos: o próprio README diz que é projeto de pesquisa, hoje em modo manutenção, e que **indexação é cara** (muitas chamadas LLM para extrair o grafo). Para este repo: fascinante, mas desproporcional — ver seção 7.

### 3.5 Memória de agentes (o outro uso de embeddings)

LLMs esquecem tudo entre chamadas. "Memória" em agentes = carregar o passado relevante para o prompt. A literatura divide em curto prazo (histórico do thread atual) e longo prazo (entre Sessões), com stores que combinam JSON + busca vetorial:

- **Letta (ex-MemGPT):** agentes *stateful* com memória que aprende ao longo do tempo; o repo original documenta memória hierárquica com recall vetorial ([repo](https://github.com/cpacker/MemGPT)).
- **LangChain/LangGraph:** short-term = `checkpointer` por thread; long-term = `store` (namespace + key + JSON) com busca por filtro e similaridade vetorial via `IndexConfig(embed=...)` ([short-term](https://docs.langchain.com/oss/python/langchain/short-term-memory), [long-term](https://docs.langchain.com/oss/python/langchain/long-term-memory)).
- **LlamaIndex:** memória via índices + `retrievers`, `routers` e `postprocessors` (rerank) antes do sintetizador ([LlamaIndex RAG](https://developers.llamaindex.ai/python/framework/understanding/rag/)).
- Padrões derivados: **cache semântico** (pergunta nova parecida com pergunta antiga → reutiliza resposta, economiza LLM e fetch), **memória episódica vs semântica** (episódios: "na Sessão 12 achamos X"; semântica: "Obscura usa CDP"), **Agentic/Hierarchical RAG** (o agente decide quando e o que buscar, em vez de um retrieve único).

---

## 4. Memória e RAG em Agentes: como funciona na prática

O loop genérico de um agente com memória vetorial:

```
nova pergunta
  │  1. embed(pergunta)
  ▼
busca na memória (top-K episódios/fatos) + histórico recente do thread
  │  2. monta contexto: [fatos recallados] + [últimos N turnos] + [pergunta]
  ▼
LLM decide: responder? buscar ferramenta/web? salvar memória?
  │  3. executa, responde, e escreve de volta (store.put / append JSONL)
  ▼
próximo turno enxerga o que este aprendeu
```

Três decisões de design que separam sistemas bons de ruins:

1. **O que indexar:** não indexe tudo. Indexe *sínteses* (resumo da Evidência, conclusão do Turno), não dumps brutos. Vetor de texto sujo recupera sujeira.
2. **Quando esquecer:** sem TTL/consolidação, a memória vira pântano. O LangGraph organiza por `namespace` (ex.: por usuário/app) justamente para filtrar antes de comparar vetor ([long-term](https://docs.langchain.com/oss/python/langchain/long-term-memory#memory-storage)). Sessões antigas pedem expiração ou compactação.
3. **Filtro antes do vetor:** restrinja por metadados (mesma Pesquisa? mesmo domínio? data válida?) e só então ordene por similaridade. Qdrant faz filtro+vetor numa passada via payload index ([Qdrant overview](https://qdrant.tech/documentation/overview/#payload-indexes)); em JSONL faz-se o filtro em código antes do ranking.

---

## 5. Exemplos inteligentes em Agentes (4 cenários)

### Cenário 1 — Dedup semântica de Evidências (o "já vimos isso")

**Problema:** entre Turnos, o agente recoleta a mesma página com URL ligeiramente diferente (`?utm=...`, http vs https) ou dois sites republicam o mesmo texto. Dedup por URL falha.
**Solução:** ao coletar cada Evidência, gerar embedding do trecho e comparar com as Evidências da Sessão; similaridade > 0.93 → marcar como duplicada e não re-sintetizar.
**Por que embeddings:** paráfrases e republicações têm texto diferente e sentido igual — só vetor pega.
**Armadilha:** threshold alto demais junta coisas distintas; registre o par (url_a, url_b, score) para auditar.

### Cenário 2 — Rerank de Evidências antes da síntese (o "menos é mais")

**Problema:** 40 Evidências coletadas, prompt da síntese estoura ou o LLM se distrai com trecho fraco (o problema "lost in the middle").
**Solução:** retrieval traz 40 → reranker (Cohere ou `bge-reranker` local) escolhe top 6–8 → só esses entram na síntese, cada um com URL.
**Por que funciona:** separa *recall* (não perder nada) de *precisão* (só o melhor no prompt). É o padrão [retrieve → postprocess → synthesize](https://developers.llamaindex.ai/python/framework/understanding/rag/#querying-stage).
**Custo:** 1 chamada de rerank por Turno final — barato perto de re-sintetizar com contexto gigante.

### Cenário 3 — Cache semântico de Pesquisas passadas (o "já respondemos isso")

**Problema:** usuário repete Pesquisa com outras palavras ("o que é Obscura?" → "me explica a engine Obscura"). Hoje: re-busca tudo na web.
**Solução:** embed do objetivo da Pesquisa + das conclusões; nova Pesquisa com similaridade > 0.9 contra Sessão passada e fontes ainda frescas → reutilizar síntese (marcando reuso) ou só atualizar o que mudou.
**Por que embeddings:** match exato de string jamais ligaria as duas frases.
**Armadilha:** dado web envelhece — combine similaridade com `fetched_at` (frescura). Cache sem validade vira resposta velha confiante.

### Cenário 4 — Memória de Sessão multi-turno que sobrevive ao JSONL (o "continuar de onde parei")

**Problema:** Sessão com 15 Turnos não cabe no prompt; truncar o início perde decisões ("já descartamos a doc X porque era de 2022").
**Solução em 2 camadas:** (a) *resumo rolante* — ao fechar cada Turno, o agente escreve 3–5 linhas (achado, decisão, pendência) num `resumo.md` da Sessão; (b) *recall vetorial* — embeddings desses resumos + das Evidências permitem ao Turno 16 perguntar "o que já sabemos sobre preço?" e recuperar só o relevante, em vez de reler 15 Turnos. É a receita MemGPT/Letta ([repo](https://github.com/cpacker/MemGPT)) e LangGraph store ([docs](https://docs.langchain.com/oss/python/langchain/long-term-memory)) adaptada para arquivo.
**Por que não só aumentar o contexto:** contexto longo custa mais, é mais lento e o modelo se distrai ([motivação documentada no LangChain](https://docs.langchain.com/oss/python/langchain/short-term-memory#overview)).

### Cenário 5 (bônus, avançado) — Avaliação de relevância como juiz

**Problema:** como saber se o Turno 3 buscou bem sem ler tudo?
**Solução:** um passo "juiz" que pontua cada Evidência contra o objetivo (0–2: irrelevante/parcial/central) e decide: sintetizar, buscar mais, ou reformular a query do próximo Turno. Com embeddings dá para pré-ordenar antes do juiz LLM, barateando o passo.
**Quando usar:** só quando o loop multi-turno já funciona — é otimização, não fundação.

---

## 6. O que faria sentido NESTE repo

### 6.1 De onde partimos (grounded nos docs do projeto)

- **Produto:** CLI que roda uma **Pesquisa** até síntese final, evoluindo para API sem rewrite ([MISSION.md](../../../MISSION.md)).
- **Vocabulário:** Pesquisa (objetivo) → Sessão (execução multi-turno, recuperável) → Turno (planejar → buscar → ler → sintetizar) → Evidência (conteúdo + URL + momento da coleta) ([CONTEXT.md](../../../CONTEXT.md)).
- **Restrições vigentes:** Rust + tokio ([ADR-0001](../../adr/0001-rust-linguagem-ancora.md)); **sem DB: JSONL por Sessão (`sessions/<id>.jsonl`) + cache de fetch em disco por URL normalizada**, migrando para SQLite só "quando doer" ([ADR-0002](../../adr/0002-sem-db-so-arquivos.md)). Formato do Turno deve ser migrável 1:1 para futura tabela `turns`.
- **Implicação direta:** qualquer proposta com servidor vetorial, docker ou conta gerenciada **exige revisitar o ADR-0002** — precisa "doer" o suficiente para justificar. As propostas abaixo respeitam isso em ordem crescente de invasão.

### 6.2 Matriz esforço × valor (ordenada por "faça primeiro")

| # | Ideia | O que é, em termos do repo | Esforço | Valor | Veredito |
|---|---|---|---|---|---|
| 1 | **Dedup/normalização por URL + hash (sem vetor)** | canonicalizar URL (tira tracking, normaliza scheme) + hash do texto; Evidência repetida entre Turnos é marcada, não re-sintetizada | P (horas) | Alto | **Faça primeiro.** É o ADR-0002 levado a sério; resolve 80% das duplicatas sem embedding nenhum |
| 2 | **Rerank de Evidências antes da síntese** | no fim de cada Turno (ou antes da síntese final), ordenar Evidências por relevância ao objetivo da Pesquisa e cortar top-N | P–M (chamada de API Cohere/Jina ou `bge-reranker` local) | Alto | **Melhor custo-benefício com modelo.** Comece por heurística (posição + overlap de termos) e evolua para reranker de verdade com o harness de avaliação (item 5) |
| 3 | **Dedup semântica (embedding) entre Turnos** | embedding por Evidência (BGE-small local ou `text-embedding-3-small` via API); similaridade > threshold → agrupa | M (guardar vetor + `cosine` em Rust; sem servidor) | Médio-Alto | **Faça quando o item 1 não bastar** (paráfrases/republicações). Vetores podem morar num `.jsonl` lado a lado ou `sqlite-vec` — sem quebrar "sem DB" |
| 4 | **Cache semântico de Pesquisas/Sessões** | embedding do objetivo + síntese final; nova Pesquisa similar e fresca reutiliza | M (índice por Sessão + política de frescura por `fetched_at`) | Médio | **Faça depois do 2–3.** Exige definir "fresco" por domínio; sem isso vira resposta velha |
| 5 | **Harness de avaliação de relevância** | 20–30 perguntas com URLs esperadas; recall@K + fidelidade a cada mudança | M (curadoria inicial, depois automático) | Alto (multiplica todos os outros) | **Faça junto do 2.** Sem isso, rerank/embedding é achismo (seção 2.6) |
| 6 | **Memória vetorial de Sessão (resumos + recall)** | resumo rolante por Turno + busca vetorial sobre resumos/Evidências | M–G (formato migrável p/ `turns`, expiração) | Médio | **Adie até Sessões longas doerem.** Hoje Sessão cabe no JSONL; complexidade prematura |
| 7 | **Busca híbrida BM25+vetor sobre Evidências** | índice invertido simples + vetor, fusão RRF | M | Médio-Baixo | **Só se queries com código/versão dominarem.** No web-search aberto, o buscador externo já faz isso |
| 8 | **sqlite-vec como índice local** | trocar o `.jsonl` de vetores por tabela `vec0` | M | Médio | **Caminho natural se o item 3 crescer.** Continua 1 arquivo, sem servidor ([sqlite-vec](https://github.com/asg017/sqlite-vec)); antecipe a migração prevista no ADR-0002 |
| 9 | **Qdrant/pgvector/Pinecone/Weaviate/Milvus** | servidor vetorial dedicado | G (infra + revisitar ADR) | Baixo hoje | **Não fazer.** Nenhum volume atual justifica; reavaliar com milhões de Evidências |
| 10 | **GraphRAG / Knowledge Graph** | extração de entidades + comunidades + QA global | G (custo alto de indexação LLM, projeto em manutenção ([repo](https://github.com/microsoft/graphrag))) | Baixo hoje | **Não fazer.** Pesquisas web pontuais raramente precisam de multi-hop com garantias; sub-buscas do loop cobrem |

### 6.3 Recomendação faseada (o caminho honesto)

1. **Fase 0 (esta semana, sem ML):** item 1 + corte top-N determinístico + registrar `score` e `motivo` em cada Evidência no JSONL (já no formato migrável para `turns`). Custo zero, aprendizado imediato.
2. **Fase 1 (quando tiver o harness — item 5):** plugar reranker (API) atrás de flag; comparar com/sem rerank no harness; guardar decisão.
3. **Fase 2 (quando paráfrase doer):** embeddings via API ou BGE local, só para dedup (item 3); vetores em arquivo lado a lado; threshold calibrado no harness. Se o arquivo virar gargalo, migrar para `sqlite-vec` (item 8) — que é exatamente a "dor" que o ADR-0002 previu como gatilho para SQLite.
4. **Não-fases:** 9 e 10 ficam estacionados até métrica pedir. Escrever isso aqui evita que daqui a 6 meses alguém proponha "vamos botar Qdrant" sem o contexto do porquê não.

### 6.4 Tradeoffs que precisam ficar explícitos

- **Custo e latência:** cada Evidência vira 1 chamada de embedding; rerank é 1 chamada por Turno final. Com API paga, Pesquisa longa fica mensuravelmente mais cara — meça tokens como a [OpenAI precifica](https://developers.openai.com/api/docs/guides/embeddings#embedding-models).
- **Privacidade:** embeddings via API enviam conteúdo para terceiro; BGE/E5 locais ([BGE](https://huggingface.co/BAAI/bge-small-en-v1.5), [E5](https://huggingface.co/intfloat/multilingual-e5-large)) evitam isso ao custo de rodar modelo em Rust (via ONNX/candle — não trivial, some ao esforço).
- **Rust:** não há ecossistema agente/LLM maduro como em Python ([consequência registrada no ADR-0001](../../adr/0001-rust-linguagem-ancora.md)); embeddings via HTTP (`reqwest` + `serde`) seguem o padrão já decidido (hand-roll sem SDK), e `sqlite-vec` tem binding Rust (`cargo add sqlite-vec` — [repo](https://github.com/asg017/sqlite-vec)).
- **Frescura de dado web:** índice vetorial de web envelhece; `fetched_at` + revalidação (ETag/Last-Modified, já previstos no ADR-0002) valem mais que modelo melhor.

---

## 7. Glossário (leigo → técnico)

| Termo | Em uma frase | Detalhe de 1 linha |
|---|---|---|
| **Embedding** | endereço numérico do significado | vetor de floats; textos parecidos = vetores próximos ([OpenAI](https://developers.openai.com/api/docs/guides/embeddings)) |
| **Vector store** | arquivo/servidor que guarda vetores e busca os vizinhos mais próximos | ex.: sqlite-vec, pgvector, Qdrant, Chroma ([Chroma](https://docs.trychroma.com/docs/overview/introduction)) |
| **Chunking** | fatiar documento em pedaços de 1 ideia | com overlap e metadados (URL, data) |
| **Chunk / Node** | cada pedaço indexado | LlamaIndex chama de `Node` ([docs](https://developers.llamaindex.ai/python/framework/understanding/rag/)) |
| **Retrieval** | trazer K candidatos rápido | recall alto, precisão média; usa HNSW ([Qdrant](https://qdrant.tech/documentation/overview/)) |
| **Rerank** | reordenar candidatos com modelo preciso | ex.: [Cohere Rerank](https://docs.cohere.com/v2/docs/rerank), bge-reranker |
| **Grounded generation** | responder só com base nos trechos + citar | se fonte não sustenta, diga "não encontrei" |
| **Hybrid search** | vetor (sentido) + BM25/keyword (termo exato) | `alpha` controla o peso ([Weaviate](https://docs.weaviate.io/weaviate/search/hybrid)) |
| **Sparse vector (SPLADE)** | vetor de pesos por palavra, BM25 aprendido | bom p/ termo raro; BGE-M3 gera denso+sparse |
| **ColBERT / late interaction** | 1 vetor por token + MaxSim | mais preciso que vetor único ([repo](https://github.com/stanford-futuredata/ColBERT)) |
| **Knowledge Graph / GraphRAG** | grafo de entidades e relações p/ multi-hop | poderoso e caro ([repo](https://github.com/microsoft/graphrag)) |
| **DPR** | ancestral do retrieval denso (2 encoders) | [arxiv 2004.04906](https://arxiv.org/abs/2004.04906) |
| **MTEB** | benchmark público de embeddings | usado p/ comparar modelos ([OpenAI](https://developers.openai.com/api/docs/guides/embeddings#embedding-models)) |
| **Cache semântico** | reutilizar resposta de pergunta parecida | economiza fetch + LLM; exige validade |
| **Memória (agente)** | carregar passado relevante no prompt | short-term (thread) vs long-term (store) ([LangChain](https://docs.langchain.com/oss/python/langchain/long-term-memory)) |
| **HNSW** | índice aproximado de vizinhos (grafo) | rápido; base de pgvector/Qdrant ([pgvector](https://github.com/pgvector/pgvector)) |
| **Cosine similarity** | similaridade pelo ângulo dos vetores | com vetores normalizados, equivale a dot/L2 p/ ranking |

---

## 8. Fontes (primárias)

**Embeddings — modelos e APIs:**
- OpenAI — Vector embeddings (modelos `-3`, dimensões, preços, usos): https://developers.openai.com/api/docs/guides/embeddings
- Cohere — Embeddings (input_type, multilíngue, imagem, Matryoshka, compressão): https://docs.cohere.com/docs/embeddings
- Voyage — Text Embeddings (voyage-4/Code/Finance/Law, input_type, quantização): https://docs.voyageai.com/docs/embeddings
- Jina — Embedding API (v5-text/v5-omni, normalização, multimodal): https://jina.ai/embeddings/
- BAAI BGE-small-en-v1.5 (model card + MTEB): https://huggingface.co/BAAI/bge-small-en-v1.5
- intfloat multilingual-e5-large (model card + MTEB): https://huggingface.co/intfloat/multilingual-e5-large

**Vector stores:**
- sqlite-vec (vec0, Rust binding, sem servidor): https://github.com/asg017/sqlite-vec
- pgvector (operadores `<->`/`<=>`/`<#>`, HNSW, sparse/binário): https://github.com/pgvector/pgvector
- Qdrant overview (retrieval, sparse, HNSW, payload index, híbrido): https://qdrant.tech/documentation/overview/
- Weaviate hybrid search (BM25F + vetor, alpha): https://docs.weaviate.io/weaviate/search/hybrid
- Chroma intro (denso+sparse+híbrido, embutível): https://docs.trychroma.com/docs/overview/introduction
- Pinecone docs (serverless gerenciado): https://docs.pinecone.io/guides/get-started/overview
- Milvus docs: https://milvus.io/docs

**Rerank, late-interaction, sparse:**
- Cohere Rerank (v4/v3.5, 4096 tokens, JSON): https://docs.cohere.com/v2/docs/rerank
- ColBERT repo (late interaction, MaxSim, papers SIGIR'20→EMNLP'23): https://github.com/stanford-futuredata/ColBERT

**Grafos:**
- Microsoft GraphRAG repo (pipeline, comunidades, custo de indexação, manutenção): https://github.com/microsoft/graphrag
- GraphRAG paper: https://arxiv.org/pdf/2404.16130
- DPR (fundamento do retrieval denso, Karpukhin et al. 2020): https://arxiv.org/abs/2004.04906
- RAG (paper original, Lewis et al. 2020): https://arxiv.org/abs/2005.11401

**RAG e memória em agentes:**
- LlamaIndex RAG (loading→evaluation, nodes, retrievers, rerank, synthesizers): https://developers.llamaindex.ai/python/framework/understanding/rag/
- LangChain short-term memory (checkpointer por thread): https://docs.langchain.com/oss/python/langchain/short-term-memory
- LangChain long-term memory (store, namespaces, IndexConfig com embed): https://docs.langchain.com/oss/python/langchain/long-term-memory
- Letta / MemGPT repo (agentes stateful, memória vetorial): https://github.com/cpacker/MemGPT

**Docs deste projeto (grounding da seção 6):**
- `MISSION.md`, `CONTEXT.md`, `NOTES.md` (raiz) e `docs/adr/0001-rust-linguagem-ancora.md`, `docs/adr/0002-sem-db-so-arquivos.md`
