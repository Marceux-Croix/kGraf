# kGraf usage guide

The full command reference for the `kgraf` command line and the `kgraf-app` desktop
window. The [README](../README.md) has the short version.

## Build

```
cargo build --workspace
cargo test --workspace
```

The workspace needs Rust 1.95 or later. It builds two programs: `kgraf` (the command
line, `crates/kgraf-cli`) and `kgraf-app` (the desktop shell,
`crates/kgraf-shell`).

## Set up a project

Workers build and test with the network off (decision 0084), so a run needs the project's
dependencies in the local cargo cache before it starts. Answer once per project whether
kGraf may fetch them (decision 0087):

```
kgraf deps allow     # fetch from crates.io whenever a run needs it, without asking again
kgraf deps status    # the stored answer, and whether the dependencies are ready offline
kgraf deps fetch     # fetch now, once
kgraf deps refuse    # never fetch; a run that needs a fetch stops before it starts
kgraf deps reset     # forget the answer; the next run that needs a fetch asks
```

- The answer is kept in `.kgraf/dependency-fetch.json`, outside the repository's tracked
  files. It is per project.
- At every run start (the command, scheduled and tray-fired runs, the desktop app) kGraf
  checks offline whether the dependencies are ready (about a third of a second on a large
  workspace). Only when one is missing does the answer matter: allowed runs `cargo fetch` on
  this machine, outside the workers' sandbox, and says which index it contacted; refused
  stops the run with what to do.
- With no answer, `kgraf orchestrate` at a terminal asks once (once for this run, always for
  this project, not now, never for this project), and the desktop app shows the question
  as a card (Allow once, Always for this project, Not now). With nobody there to answer (a
  scheduled run, a script), the run stops before it starts and says how to answer; a
  schedule is turned off with that reason until you answer and run `kgraf schedule enable`.
  kGraf never fetches unasked.
- `--fetch-deps` and `--no-fetch-deps` on `orchestrate` and `schedule run-now` answer for
  one run and store nothing.
- A worker that adds a crates.io dependency gets it fetched when the project allows
  fetching; otherwise its build fails and says why. At most 8 fetches and 32 checks per run
  for its workers, and 3,000 packages per fetch; a fetch still running stops when the run
  ends. A refusal you store during a run (`kgraf deps refuse`) applies to the next worker
  build, even in a run started with `--fetch-deps` or "Allow once".
- Only crates.io. A git dependency or another registry in any `Cargo.toml` of the project,
  a workspace member or not, stops the fetch: run `cargo fetch` yourself if you trust it.
  `cargo fetch` downloads and unpacks; it runs no build script. kGraf's fetch never uses a
  git transport, so a crates.io index you configured as a git index cannot be fetched by it.
- With no `Cargo.toml` at the project root but one below it, nothing is checked or fetched,
  and each run start says so.
- Workers cannot merge changes to a `.cargo` folder or a `rust-toolchain` file: make those
  by hand.
- Each fetch is a network event with no tokens; `kgraf diag show` counts them.

## The code graph

Point it at a repository, including this one:

```
cargo run -p kgraf-cli -- index
cargo run -p kgraf-cli -- symbol slice_for_symbol
cargo run -p kgraf-cli -- slice slice_for_symbol --budget 1500
cargo run -p kgraf-cli -- query 'kind=function -calls-> name=stable_id'
cargo run -p kgraf-cli -- cochange --commits 500
cargo run -p kgraf-cli -- partners ROADMAP.md
```

`-C <dir>` points any command at another repository.

Only `kgraf index` creates or rebuilds the index. `symbol`, `slice`, `query`, `partners` and
`demand` open it read-only, and every other command opens an existing index without
rebuilding it. Where there is no index, or it was written by another schema version, they
stop and say to run `kgraf index`. A slice reads each node's lines from the file, and it
stops with an error when the file changed after it was indexed; run `kgraf index` first. A
name that matches more than one node is refused with the candidates listed (an exact
qualified name still picks its one node).

An index pass skips `target`, `node_modules`, `.git` and similar tool output at any depth,
and `build`, `dist`, `venv` and the like only at the top level, so `src/build/` is indexed.
It prints the excluded directories, files over 2 MiB, files whose bytes are not UTF-8, and
any symbolic link or unreadable file whose indexed rows it kept.

The graph chunks Rust, Python, TypeScript, TSX, JavaScript and Markdown with kGraf's own
rules, and Go, Java, C, C++, Ruby, C# and PHP from each tree-sitter grammar's own tags
query (the tags tier; a slice marks those members as resolved by name only). A call links
only to a definition in its own language family. When the extractor for a language
changes, the next index re-derives that language's files and says so.

## What the roadmap says remains: presumed work nodes

```
cargo run -p kgraf-cli -- index                              # also reads the roadmap
cargo run -p kgraf-cli -- plan                               # list presumed work and its state
cargo run -p kgraf-cli -- plan --from-roadmap --source docs/milestones.md
```

Every `kgraf index` reads the project's roadmap documents (ROADMAP, TODO, PLAN,
MILESTONES, BACKLOG or TASKS, `.md` or `.markdown`, in the root or `docs/`) into
**presumed** work nodes, with no model call: status tables, task lists and status
headings. Each node shows presumed done, partly done or remaining, derived from what the
document claims and whether the code it names exists, and says where the two disagree.
Re-running updates the same nodes; it never duplicates them, never deletes them, and never
overwrites a node you changed. When an approved plan takes an item over, the presumed node
keeps its identity and the galaxy draws the running work on it. `--no-presume` skips the
read; `kgraf plan --from-roadmap` with no roadmap found is an error.

## Providers, spend and the ledger

[MODEL-SUPPORT.md](MODEL-SUPPORT.md) lists which routes and model kinds have run against the live
service and which are built but untested. Read it before you choose a route.

Set a route's key **in your own shell**; it is never read from a file:

```
export KGRAF_KIMI_API_KEY=...        # the route's own variable; never paste a key into a chat
cargo run -p kgraf-cli -- providers
cargo run -p kgraf-cli -- call --provider kimi --model <model-id> --record kimi.json
cargo run -p kgraf-cli -- spend --prices prices.json
cargo run -p kgraf-cli -- unreconciled
cargo run -p kgraf-cli -- unreconciled resolve <call-id> --input 120 --output 30
cargo run -p kgraf-cli -- unreconciled resolve <call-id> --no-charge
cargo run -p kgraf-cli -- unreconciled resolve <call-id> --output 30 --abandoned
cargo run -p kgraf-cli -- ledger verify
cargo run -p kgraf-cli -- demand
```

Every model call goes through the ledger. `spend` totals it (in dollars with a price
table; `total unpriced` when no recorded model has a price), `unreconciled` lists calls
whose spend is not settled and shows the partial usage recorded for each (a lower bound),
`unreconciled resolve` closes one with the usage the provider's own invoice shows (or
`--no-charge` when the provider confirms it billed nothing). A field you leave out keeps the
partial figure already recorded. A call listed as `in_flight` may still be live, so resolve
refuses it unless you pass `--abandoned` to state that its process is gone. There is no bulk
resolve: each call needs its own figure. `ledger
verify` walks the
hash chain, and `ledger redact <call-id> --reason ...` removes a payload captured by
accident while the chain still verifies. `demand` reports induced demand: whether
runs offered workflow templates started more work than runs without. It claims no saving.

### AWS Bedrock

The `bedrock` route speaks Bedrock's Converse and ConverseStream operations, signed with
AWS Signature Version 4 (decision 0091). It is listed as `unverified`: it has been tested
against recorded-shape responses only, never against the service. It takes the standard AWS
configuration, not a `KGRAF_*_API_KEY` variable. Set it in your own shell. A Bedrock API
key is the simplest credential; kGraf sends it as a bearer token, unsigned, and it wins over
access keys when both are set:

```
export AWS_BEARER_TOKEN_BEDROCK=...   # a Bedrock API key; still needs a region, below
```

Or use access keys:

```
export AWS_ACCESS_KEY_ID=...
export AWS_SECRET_ACCESS_KEY=...
export AWS_SESSION_TOKEN=...          # only for temporary keys
export AWS_REGION=us-east-1           # or AWS_DEFAULT_REGION, or `region` in the profile
```

Or name a profile with static keys in `~/.aws/credentials` (`AWS_PROFILE=work`;
`AWS_SHARED_CREDENTIALS_FILE` and `AWS_CONFIG_FILE` move the files). The variables win over a
profile. A profile that uses SSO, role assumption, a credential process or web identity is
refused, because each needs a network exchange kGraf does not make: export temporary keys
first, for example with `aws configure export-credentials --profile work --format env`.

Credentials and region are read again on every call. `kgraf providers` shows the route, and
a missing key or region is reported when the route is checked, before any call. The model id
you configure is the Bedrock model id or inference profile id (for example one ending in
`-v1:0`); the adapter refuses a model outside the set you configured before it sends anything.

What to expect:
- Input excludes cache reads and writes (additive). The reply's `totalTokens` is checked
  against the parts; a mismatch keeps the answer and adds a `kgraf_note` to the call's raw
  usage, because the call was billed.
- A reply served on a service tier other than `default` (`priority`, `flex`, `reserved`), or
  with cache writes on a TTL other than `5m`, keeps its answer and token counts, records the
  tier in the raw usage, and shows as unpriced: the price table has one rate per model.
- Redirects are not followed. A non-finite temperature is refused before anything is sent.
- Blank text is never sent (the service rejects it); an empty tool result goes as
  `(no output)`.
- A `424` reply (an error while the model processed the request) leaves the call unreconciled,
  since the service does not say it is unbilled.
- The region must be lowercase letters, digits and hyphens, because it becomes part of the host
  name.
- A model id the price table does not list (an inference profile id such as
  `us.anthropic...-v1:0` is a different key from the bare model id) shows as unpriced, never
  as zero cost.
- The service does not name the model in its reply, so a substituted model cannot be caught
  by the model check.
- Cache points are off for this route and cannot be turned on in configuration. They work on
  some models only; the service rejects them on the rest.
- A request that sets a reasoning effort is refused: Converse has no model-independent field
  for it.
- A private (VPC) endpoint cannot be set yet; the route uses
  `https://bedrock-runtime.<region>.amazonaws.com`.
## Google routes: Gemini and Vertex AI

Three routes reach Google models. All three are marked `unverified` in `kgraf providers`:
their behavior comes from Google's published references and recorded test bodies, and none
has been run against the live service. Check the recorded raw usage on the first call.

| Route | What it reaches | Credential |
|---|---|---|
| `gemini` | Gemini through its own `generateContent` API, with tool calling, streaming and a thinking control | `KGRAF_GOOGLE_API_KEY`, the key the `google` route also reads |
| `vertex-gemini` | Gemini on Vertex AI | Vertex settings, below |
| `vertex-anthropic` | Anthropic models on Vertex AI | Vertex settings, below |

The `google` route is Gemini's OpenAI-compatibility layer, which ignores parameters it does not
support. Use `gemini` when you need the thinking control or thought summaries.

**Gemini with an API key.** Set the key in your own shell and pick a model id:

```
export KGRAF_GOOGLE_API_KEY=...
cargo run -p kgraf-cli -- call --provider gemini --model gemini-2.5-flash
```

**Vertex AI (both routes).** Vertex takes an OAuth token, not an API key. Set where the
credentials come from, the project that is billed, and the region. There is no default region:
the region sets where data is processed and which models exist.

```
export KGRAF_VERTEX_CREDENTIALS=adc          # or metadata, gcloud, or a service-account key file path
export KGRAF_VERTEX_PROJECT=my-project       # optional with a key file: its project_id is used
export KGRAF_VERTEX_REGION=us-east5          # or `global`
cargo run -p kgraf-cli -- call --provider vertex-gemini --model gemini-2.5-flash
cargo run -p kgraf-cli -- call --provider vertex-anthropic --model <name>@<version>
```

Credential sources. kGraf uses only the one you name and never probes the others:

| Value | Source |
|---|---|
| `adc` | the service-account key file that `GOOGLE_APPLICATION_CREDENTIALS` names, if it is set; otherwise the gcloud user credentials file (`gcloud auth application-default login`). It does not contact the metadata server and does not run `gcloud`. |
| `metadata` | the metadata server of the Google Cloud host kGraf runs on |
| `gcloud` | the output of `gcloud auth print-access-token` |
| a path | a service-account key file |

Other credential file types, such as `external_account` (workload identity federation) files,
are not supported. A key file whose name is `adc`, `metadata` or `gcloud` is written `./adc`.
Where a kGraf variable is not set, kGraf reads `GOOGLE_APPLICATION_CREDENTIALS`,
`GOOGLE_CLOUD_PROJECT` (or `GCLOUD_PROJECT`) and `GOOGLE_CLOUD_LOCATION` (or
`GOOGLE_CLOUD_REGION`). A missing setting fails when the route is checked, naming the variable to
set. The account needs the Vertex AI User role, and each Anthropic model must be enabled for the
project in Vertex AI Model Garden. The region can be a region such as `us-east5`, `global`, or
the multi-region `us` or `eu`.

Use the Anthropic model id as Vertex lists it (`name@version`, or the bare name). kGraf accepts
the id as sent, `name-version`, and a bare name answered with its dated id, and refuses any other
model name in a response. For Gemini, a requested alias such as `gemini-2.0-flash` may be answered
as `gemini-2.0-flash-001`; kGraf accepts a numeric version suffix and refuses any other model name.

Tool schemas: Vertex accepts an OpenAPI subset of JSON Schema. kGraf converts a tool's schema and
refuses the call, before sending, when the schema uses a keyword the subset cannot express
(`$ref`, `oneOf`, `allOf`, or several non-null types), an `enum` whose values are not all strings,
an object with no properties (a free-form object), or a node with no type that has none implied.
The error names the tool and the place in its schema. An MCP tool whose schema is free-form
cannot be offered to a Vertex route. Function names start with a letter or an underscore, hold
only letters, digits, underscores, dots, colons and dashes, and are limited to 64 characters on
Vertex and 128 on the Gemini Developer API. A `-latest` model alias is refused: set the concrete
model id it resolves to.

A Vertex route that uses your `gcloud auth application-default login` credentials sends the quota
project that `gcloud` recorded in that file (`x-goog-user-project`), so Vertex AI is billed to that
project. Set it with `gcloud auth application-default set-quota-project`. kGraf also registers the
secrets in the credentials file it reads (the private key, key id, refresh token and client
secret) so they are redacted if a tool prints the file.

Reasoning effort: `gemini-2.5` models take a token budget, capped at half of the output limit,
and `gemini-3` models take a level. A level the model does not document is refused (for example
`medium` on `gemini-3-pro-preview`, which documents `low` and `high`). A token budget below the
model's minimum (512 on `gemini-2.5-flash-lite`, 128 on the others) is refused, so a Flash-Lite
output limit under 1,024 tokens cannot carry a reasoning effort. A model family kGraf does not
know is refused when an effort is set.

What to expect from usage on the Gemini routes: thinking tokens are counted apart from the reply
and shown as reasoning output; cached prompt tokens are shown as cache reads; a reported total
above the counted parts is billed as reasoning, and a total below them stops the call and leaves
it in `unreconciled` with the counted figures. A turn the service blocks or refuses (a safety
block, a malformed function call) is billed and recorded, then reported as an unusable turn with
Google's reason. Gemini 3 models require the thought signature that came with a tool call to go
back with it. kGraf stores the signature with the tool call in the transcript, so a resumed
session or a new worker process sends it back. A signature is valid only for the model that made
it, so kGraf sends it only back to the same route and model. A tool call that came from another
route or model has no usable signature, and kGraf sends Google's documented skip value for it on
Gemini 3 models. A signature on a reply's text is kept the same way.

## Azure OpenAI

Two routes reach models deployed on an Azure OpenAI resource (decision 0092): `azure-openai`
(the Chat Completions body) and `azure-openai-responses` (the Responses body). Both are marked
`unverified` in `kgraf providers`: they have been tested against bodies written from Microsoft's
documentation and never against the service. Set the four settings in your own shell:

```
export KGRAF_AZURE_OPENAI_API_KEY=...               # the resource's key, sent as `api-key`
export KGRAF_AZURE_OPENAI_RESOURCE=my-resource      # the name alone: my-resource.openai.azure.com
export KGRAF_AZURE_OPENAI_API_VERSION=v1            # or a dated version for Chat, such as 2024-10-21
export KGRAF_AZURE_OPENAI_DEPLOYMENTS=my-chat=gpt-4o-2024-08-06,my-mini=gpt-4o-mini-2024-07-18
cargo run -p kgraf-cli -- call --provider azure-openai --model my-chat
```

The model you pass is the **deployment name**. The service names the underlying model in its
reply, so `KGRAF_AZURE_OPENAI_DEPLOYMENTS` maps each deployment to the exact model string the
reply carries, dated suffix included. A deployment with no mapping fails when the route is
built. A reply naming any other model stops the call and leaves its spend in `unreconciled`;
the error shows both strings, so the first mismatch tells you what to write in the mapping.

`v1` uses `https://<resource>.openai.azure.com/openai/v1/...` and sends the deployment as
`model` in the body. A dated version uses the older address, with `?api-version=` on the
request and the deployment in the path, and is available for Chat Completions only, from
`2024-09-01-preview` on (the first version with stream usage and `max_completion_tokens`);
a date without `-preview` must be `2024-10-21` or later. The check is a floor: a version that
does not exist fails on the wire. The
Responses route takes `v1` alone, because Microsoft documents the Responses body only at
`/openai/v1/responses`. The resource must be the bare name (Foundry project hosts such as
`*.services.ai.azure.com` are not supported), a deployment name cannot be made of dots alone,
and the version must be `v1` or a real date such as `2024-10-21` or `2025-04-01-preview`;
anything else is refused when the route is checked, before a request is built.

What to expect:
- Usage is inclusive, as on OpenAI. Cached prompt tokens show as cache reads and reasoning
  tokens as reasoning output.
- A completion the content filter cut, a prompt the filter flagged, a Responses completion
  marked blocked, or a reply whose filters did not run (an `error` in `content_filter_results`)
  is billed and recorded, then reported as an unusable turn with the reason (`content_filter`,
  `prompt_filter_results` or `content_filter_results_error`). A prompt the service blocks
  outright is an HTTP 400 that billed nothing.
- The output limit is sent as `max_completion_tokens`, so reasoning deployments work on both
  routes.
- Rate limits and 429s are tracked per deployment, from the `x-ratelimit-*` headers and
  `retry-after-ms`. A route with several deployments uses the tightest limit, and a 429 on
  any of them lowers the route's rate.
- Deployments that Azure updates automatically, and model-router deployments, cannot be used:
  their replies name a model that changes, and the mapping holds one string. Deploy a fixed
  model version and map it.
- Cost shows as unpriced unless a price is set for the deployment. An Azure route is never
  priced by a bare model name, since a deployment name is your own choice. Price a deployment
  with a `<deployment>@azure-openai` entry (or `@azure-openai-responses`) in your price file.
  `kgraf spend --prices` and `kgraf schedule list --prices` follow the same rule. The bench
  prices each deployment from the model it is mapped to, using the vendored table's
  `azure/<model>` entries when they exist. Those are standard-deployment rates: a data-zone,
  regional or provisioned deployment bills differently, so set its price in your price file.
- Microsoft Entra ID tokens and the sovereign clouds (`openai.azure.us`, `openai.azure.cn`)
  are not supported. Only the key is.
- Embeddings: both routes also serve the embedding kind, per deployment
  (`--model-route embedding=azure-openai:my-embed`, with `my-embed=text-embedding-3-small` in
  `KGRAF_AZURE_OPENAI_DEPLOYMENTS`). `v1` posts to `/openai/v1/embeddings` with the deployment
  as `model`; a dated version posts to `/openai/deployments/<deployment>/embeddings`, and any
  dated version is accepted for embeddings. The reply must name the mapped model, as for chat.
  A request carries at most 2,048 texts and 300,000 tokens; a longer list is split. Prices
  follow the same `<deployment>@<route>` rule. Unverified, as the chat routes.

## Models of every kind: typed decisions, embeddings, rerank

Every route has a model kind (decision 0089). `kgraf providers` lists the kinds each catalog
route serves in its KINDS column.

| Kind | What it returns | Served by | Tool |
|---|---|---|---|
| `generative` (default) | prose and tool calls | every route except `systemone` and `tei` | none: it runs workers and the planner |
| `typed-decision` | a value in a declared answer space, with a probability (`/v1/systemone`) | `systemone` (local, default `http://localhost:8000`) | `decide` |
| `embedding` | one vector per text | `openai`, `google`, `llama-cpp` (`/embeddings`); `ollama` (its native `/api/embed`); `tei` (its native `/embed`); `azure-openai`, `azure-openai-responses` (per deployment) | `embed` |
| `rerank` | relevance scores of documents against a query | `together`, `llama-cpp` (`/rerank`); `tei` (its native `/rerank`) | `rerank` |

A route serves a kind only where its vendor documents the endpoint at the route's address.
Moonshot (`kimi`, `kimi-code`), DeepSeek, z.ai, Alibaba (`qwen`), xAI (`grok`, `grok-chat`),
and Meta serve generative work only here. Together serves rerank but not embeddings: its
embeddings reply carries no token count, so every call would be billed and then refused.
Cohere's rerank endpoint is not supported: it bills in search units, not tokens. Only
embeddings on `openai` have been checked against the live service; every other claim is
marked unverified, and a reply field the vendor's page does not show is not relied on as
verified.

Together's rerank replies name a serving model of their own rather than the model you asked
for (its reference shows this, with no rule relating the two), so on `together` the rerank
reply's model name is not checked. Every other route still refuses a reply naming another
model.

`tei` is a local Text Embeddings Inference server (default `http://localhost:8080/v1`, the
same port llama.cpp uses; run one of them elsewhere). It serves one model, so its replies
name none and kGraf cannot check that the model answering is the one you named.

`--route` and `--planner` are always generative. Declare a route of any other kind as
`kind=route:model`, repeatable:

```
export KGRAF_OPENAI_API_KEY=...      # the openai route's own variable, as for a generative route
cargo run -p kgraf-cli -- orchestrate --route kimi:<model> --prices prices.json \
  --model-route embedding=openai:text-embedding-3-small \
  --model-route rerank=llama-cpp:<reranker> \
  --model-route typed-decision=systemone:jev-latest \
  --instruction task.txt
```

In `.kgraf/run.json`, the same list is `model_routes`:

```json
{
  "routes": ["kimi:<model>"],
  "prices": "prices.json",
  "model_routes": ["embedding=openai:text-embedding-3-small"]
}
```

The desktop app's Settings > Providers lists every route of `run.json` with its role and kind,
and each model route with the tool that reaches it.

- **Credentials** are the route's own: an `embedding=openai:...` route reads
  `KGRAF_OPENAI_API_KEY`; the local `systemone`, `ollama`, `llama-cpp` and `tei` routes take
  none. A missing key fails the run's readiness check and names the variable.
- **A local route must be running.** At the start of every run, each local route's endpoint
  (planner, worker or model route: `ollama`, `llama-cpp`, `systemone`, `tei`) must accept a
  connection, or the run fails before it builds anything:
  "route `ollama` is unreachable at http://localhost:11434/v1: ...; start its server or
  remove the route". Hosted routes are not dialed at start.
- **Accounting.** Every call is a ledger row. Embedding and rerank calls record their input
  tokens; a typed decision records what the server reports. Add the model to the price table
  to see its cost; without a row it is shown as unpriced, never as $0. A priced call too
  small to cost one micro-dollar shows as $0.000001, and totals add the exact amounts before
  rounding, so many small calls total what they cost. A reply counting zero tokens for
  non-empty input is treated as a reply without a count: the call is kept as unreconciled,
  never settled as free. The run's reported spend includes every model-tool call, the
  orchestrator's own and each worker's.
- **Use.** The planner is told the run's model routes by kind, and may call their tools
  itself while it plans (at most four rounds and eight tool calls a round, each call on the
  ledger as the orchestrator's; stopping the run cancels a call in flight, and a failed call
  is listed in the run's notices). A run that declares a model route puts its tool (`decide`, `embed`,
  `rerank`) in every worker's tool set, so any worker may call it; the tool set stays the same
  for every contract so workers on one model share one cached prompt prefix. A contract lists
  a tool in its `tools` where its work relies on it. Nothing calls one by default: the
  orchestrator or a worker chooses to.
- **`embed`** takes at most 32 texts. Given a `query`, it returns the texts ranked by cosine
  similarity to it. Without one, it returns the vectors while they fit in 32 KB, else each
  text's cosine similarity to every other. Models that expect a marker on queries and
  documents (for example `query: ` and `passage: `) take it in `query_prefix` and
  `text_prefix`. **`rerank`** takes at most 256 documents, or fewer where the route's server
  takes fewer (32 on `tei`); the tool's description names those routes, and a longer list is
  refused before any call.
- **Input limits.** A longer list of texts (from the orchestrator) is split into requests of
  at most 2,048 texts and about 100,000 tokens (32 texts on `tei`, 100 on `google`, 2,048
  texts and 300,000 tokens on the Azure routes), each its own ledger row.
  An empty text is refused before sending. For a text longer than the model's input limit,
  kGraf asks Ollama and TEI to refuse it rather than cut it, and OpenAI documents a refusal. The
  other routes document no refusal (Google's embeddings guide says it cuts an over-long input
  without an error), and kGraf cannot see a cut the server does not report, so keep texts
  within the model's limit there. A rerank reply with more results than
  `top_n` is cut to the best ones.
- **Refused, never rerouted.** A kind prefix on `--route` or `--planner`, a model route with
  no kind, a route that does not serve its kind (`embedding=anthropic:...`), and a tool call
  naming a route of another kind all fail with the reason. One `route:model` may be declared
  as two kinds (`embedding=llama-cpp:m` and `rerank=llama-cpp:m`).
- **Rate limits.** `--rate-limit ROUTE=...` also paces a model route's calls, on the same
  limiter as that route's worker calls. An embedding or rerank model's rate-limit headers are
  recorded under that model (OpenAI states limits per model), so they do not overwrite the
  chat model's; the machine's capacity reads both and uses the tighter. A route's 429 replies
  are recorded for the machine's learned rate, as for chat. Model-route calls are left out of
  a route's round statistics, which measure worker rounds.
- Verified live: one embeddings call of one text on the `openai` route (2026-09-29), which
  checked the request, the model echo, the vector and the usage record. Request batching, the
  vector-width check, per-model rate-limit records and the zero-count rule came later and are
  tested against recorded replies only, as is every other kind and route here.

### Connecting a System-One model

A System-One model answers typed questions about a state you give it. kGraf speaks the
`/v1/systemone` wire (decision 0042) to any server that serves it, and does not care which
model or hardware sits behind it. To use one:

1. Start a server that serves `POST /v1/systemone` at `http://localhost:8000`. The `systemone`
   route's address is fixed in the catalog. To reach a server on another machine, forward its
   port to `localhost:8000`. The route sends no credential, so put nothing on the server that
   needs one.
2. Declare the route: `--model-route typed-decision=systemone:<model>`, or
   `"model_routes": ["typed-decision=systemone:<model>"]` in `.kgraf/run.json`. `<model>` is
   sent as `model`; the server's default alias is `jev-latest`.
3. Add the model to the price table if it has a price. Without a row its calls show as
   unpriced. A local model can be priced at zero, which says nothing about compute.

The request body is `{"state": "<text>", "model": "<model>", "questions": {"<id>": {"type":
"noul" | "choice" | "score", "instructions": "<prompt>", "criteria": ...}}}`. The reply is
`{"answers": {"<id>": {"type": ..., ...}}, "usage": {"input_tokens": n, "output_tokens": n}}`,
with one answer per question id. A `noul` answer is a probability, a `choice` answer names one
of the given options, and a `score` answer is an expectation over the given levels. An answer
outside its declared space, or a missing or extra question id, is refused. A typed answer is
not a verified answer.

The model is used only where something calls the `decide` tool: the orchestrator while it
plans, or a worker that lists it. No part of a run calls a decider on its own (decision 0045),
and none supplies the collision expectation. The client exchanged a request and a reply with a real server in an earlier probe
(decision 0045); the route itself is checked against recorded replies only.
For code that wants a decider directly, `kgraf_decide::SystemOneClient` implements the
`Decider` trait; `FakeDecider` stands in for tests.

## A worker, and an orchestrated run

```
cargo run -p kgraf-cli -- worker --provider kimi --model <model-id> --brief brief.txt
cargo run -p kgraf-cli -- orchestrate --route kimi:<model-id> --route openai:<model-id> \
    --instruction task.txt --prices prices.json
cargo run -p kgraf-cli -- orchestrate --resume <run-id> --route kimi:<model-id> \
    --prices prices.json
```

- `worker` runs one worker in its own worktree under `.kgraf/workspaces/` and commits
  its work to `refs/kgraf/attempts/<worker>-<n>`; it never merges.
- `orchestrate` decomposes the instruction into contracts, dispatches workers across the
  routes, verifies each attempt with `--verify` (default `cargo test --workspace`), runs
  quality gates (discovered, or `--gate-command`), and merges verified work onto its own
  branch `kgraf/<run>`. Your checked-out branch never moves.
- kGraf sets no spend limit (decision 0080): set one in your provider's console. Every call
  is still on the ledger, and `kgraf spend` totals it. The concurrency caps are `--max-items`
  (default 4) and `--max-agents` (default 8).
- A model the price table does not list still runs. The ledger keeps every call, and the
  report and `kgraf spend` name its spend as unpriced (a local model as local, unpriced),
  never as zero dollars. Routing never refuses or skips a route for lacking a price. A
  price entry named `<model>@<route>` applies to calls made on that route and wins over the
  model's own entry. A model whose vendor bills a higher rate above an input size is priced
  only up to that size from the bundled catalog; a larger call is reported as unpriced, not
  costed at the base rate.
- `orchestrate` exits non-zero unless the run completed: a run that ends blocked or
  stopped is a failure to a script or a scheduler.
- A planner answer that is not the JSON asked for, or a plan that breaks a rule, is asked
  for again (at most twice, each ask on the ledger) with the refusal as a note, before the
  run stops. A plan that the interface checks hold before any work started (a handshake
  mismatch, or a check the last answer failed) is sent back to the planner once with the
  failures before the run ends on the hold; the re-planned contracts are named `re1.<id>`.
- `--rate-limit ROUTE=RPM[/BURST][@MAX_WAIT_MS]` limits one route's call rate; a call that
  would wait too long fails loudly.
- Every run indexes the working tree before it starts (incremental: about 65 ms warm and
  about 7 seconds cold on an 800-file repository), and reindexes a contract's files after
  its merge, so workers read graph slices without a separate `kgraf index`. A graph that is
  still empty is reported as a notice.
- **Worker worktrees and their builds** (decision 0088). Each worker builds in its own
  worktree, and a compiled project builds its own full build folder there (a Rust `target`
  folder can be several gigabytes). By default kGraf deletes a worker's worktree, with the
  build output in it, once nothing needs it: an attempt that is merged, losing, refused,
  empty or failed, a conflict its resolver settled, and a merged writer subagent's.
  Where it is set, one place per surface:
  - `kgraf orchestrate` and `kgraf schedule run-now`: give `--keep-worktrees`. These commands
    do not read `run.json`.
  - The desktop app: Settings > General > "Delete worker worktrees after use" (on by
    default) decides for every run the app starts, box and scheduled. It applies to runs
    that start after the change.
  - `run.json` does not set it: a run file with a `worktrees` key is refused, and the error
    names the switch and the flag.

  Keeping keeps every attempt's worktree and builds, for you to inspect or reuse (a merged
  attempt's work is on the branch as well); `kgraf clean <worktree>` removes a kept one. A
  kept attempt is never continued as live work. Conversation, gate, probe and interface-stub
  worktrees hold no attempt and are removed either way. A worktree that a live worker, a
  stopped run, a lost lease, a pending merge, a later part of the same work (a handoff
  continues in its parent's worktree) or a conflict resolver still needs is never deleted,
  whichever way this is set. A worktree that cannot be deleted stays, and the run's report
  says which one and why (one warning each). This includes a worktree a process started by a
  build or test still runs in (a daemon that detached itself): kGraf does not kill it, and
  removes the worktree at its next start once that process has ended, or `kgraf clean`
  removes it. A worker's build folder lives in the worktree's
  scratch directory and goes with it.
- An attempt that changes no file is accepted only when its check passes, and it is always
  reported. It is refused when a declared Rust write target holds only `todo!` or
  `unimplemented!` bodies (decision 0086; `unreachable!` is not a marker), and an attempt
  that did change files is refused when a declared target it left untouched held only such
  bodies before the run.
- Workers can build and test their own work (decision 0084). The build and test allowlist
  runs without approval, inside the sandbox with the network off: by default `cargo build`,
  `cargo check`, `cargo test`, `cargo clippy` and `cargo fmt --check`, each under argument
  rules (for example, no `--manifest-path` outside the worktree, no `--config`, no `-Z`,
  no `--target-dir`), plus the run's own `--verify` and `--gate-command` commands exactly
  as configured. Set `build_commands` in `.kgraf/worker.json` to change the list (`[]`
  turns it off). A worker with a declared write scope (every orchestrated attempt) runs
  these and no other shell command that can write; the verifier still refuses any commit
  that changes a file outside the declared writes. The list is on unless `build_commands`
  says otherwise, for attended workers too, and a cargo command that breaks an argument rule
  is refused outright, not asked about.
- A worker whose call is refused by policy, or whose instruction lacks something it needs, is
  told to report it and stop (`report_shortfall`, decision 0093). Your permission mode decides
  what happens next (see "Permission modes" below). An instructions report amends the contract
  once: the next attempt's brief carries the report, and a second one holds the contract for
  you. A contract is re-dispatched at most twice for shortfalls; a grant that did not help
  holds. Every grant and hold is in the run's trace (`shortfall` and `permission` events), and
  each grant is kept on the contract with who asked, why, and who granted it.
- Permission modes (decision 0094). One setting decides who answers a permission request past
  the base policy, for a worker's approval ask and for a shortfall report alike:
  - **Manual**: you answer every request.
  - **Accept edits**: file edits are granted (a file tool, a contract's write scope widened to a
    file it needs, and `mkdir`, `touch`, `mv`, `cp`); commands, network and MCP tools ask you.
  - **Auto**: what your allow set names is granted (`[allow] programs`, `tools`, `paths` in
    `permissions.toml`, or `--allow PROGRAM` and `--allow-path PATH`); anything else asks you.
  - **Orchestrator decides**: the orchestrator grants or refuses each request against its
    contract and never asks you. It refuses after two re-dispatches, when a grant did not help,
    for a tool, and for a path outside the repository or one no worker may write.
  - **Bypass**: everything is granted without asking: workers' shell commands run under a write
    scope, except the few the policy forbids. It offers no protection against prompt injection
    or unintended actions. Workers may also write the repository's git directory (a patch to
    `.git/info/exclude`, `.git/config` or a ref, or a `git` command in the shell) and kGraf's
    settings files `.kgraf/run.json` and `.kgraf/worker.json`. Before the first such write,
    kGraf copies the refs, config and index (at most 32 MiB) or the settings file; after each
    write it checks that the repository still reads (every ref resolves, the config parses,
    and `git fsck --connectivity-only` when the attempt ends) and that the settings file still
    loads. A write that fails its check is put back, and the attempt fails with the check's
    output and is not merged; every write, kept or put back, shows in the run's trace and the
    activity card. The ledger, the trace, run records, transcripts, the graph and the rest of
    `.kgraf` stay refused in every mode, as do `.git/objects`, the `.git` link file,
    `.gitconfig` and a nested `.git`. In Bypass only, workers may also write `.gitattributes`,
    `.gitmodules` and files under `.githooks`. These are ordinary files of your project: a
    contract must declare them like any file, the verifier checks them against that list, and
    they merge as plain data, with no copy taken first (git history keeps every version).
    kGraf's own git never runs what they select (see "kGraf's own git" below), but your git
    does: a merged `.githooks/pre-commit` runs at your next commit if your `core.hooksPath` is
    `.githooks`. Review them as you would any change.

  In every mode, Bypass included: your deny rules block (`deny = ["shell(curl*)"]`, or
  `--deny`), your ask rules ask (`ask = [...]`, or `--ask`), deleting the filesystem root, a
  top-level directory or your home directory is refused, deleting the workspace itself or its
  `.git` asks, every hook runs, and the OS sandbox stays on. A run nobody attends (the command
  line, a scheduled run) denies whatever would ask, and the hold says how to allow it. In the
  app, a request waits in the orchestrator box until you answer it; requests raised within 2 s
  of each other arrive as one card, with Grant and Deny on each and Grant all and Deny all.

  Only you set the mode: the dropdown at the top of the orchestrator box, the user-level
  `permissions.toml` (in kGraf's configuration directory, beside the app's `settings.toml`; set
  `KGRAF_PERMISSIONS_PATH` or pass `--permissions-file` to use another), or
  `--permission-mode` on `kgraf orchestrate`. A change applies at once, to running workers and
  to requests already waiting. `.kgraf/run.json` and `.kgraf/worker.json` refuse every
  permission key, and a `permissions.toml` inside the project is refused. Bypass is offered in
  the app only after you turn on "Allow Bypass mode" in Settings, and its first use asks you to
  accept a warning; `disable_bypass = true` in `permissions.toml` refuses it everywhere; and it
  is refused when kGraf runs as root unless `KGRAF_CONTAINER_SANDBOX=1` says the process runs
  in a disposable container. On the command line, `--container-sandbox` (with that variable) is
  another name for `--permission-mode bypass`, `--may-grant` is another name for `--allow`, and
  `--allow` given without a mode selects Auto. The command prints a `BYPASS PERMISSION MODE`
  notice to stderr whenever a run starts in Bypass.
- Plan review (decision 0094). Turn on "Review the plan before workers start" beside the mode
  dropdown (or `plan_review = true` in `permissions.toml`, or `--plan-review`). After planning,
  the plan waits in the orchestrator box: each contract with what it waits on, writes and
  reads, and the graph lights what it writes (hover a contract to light its own). Approve plan
  dispatches it; Change plan sends your note to the planner and shows a new plan; Reject plan
  ends the run with nothing dispatched. Turning review off while a plan waits approves it. A run
  nobody can approve (the command line, a scheduled run) is refused before anything is planned,
  unless `--approve-plan` approves the plan for you.
- A contract whose check always passes (`--verify true`, `:`, `exit 0`), or whose worker
  finished after a policy refused a call it never ran, is printed as `unverified <id>:
  <reason>` after the `verified` line. The run still completes; the line says the check did
  not look at the work.
- While a worker's worktree has a `.cargo/config.toml`, `.cargo/config`, `rust-toolchain` or
  `rust-toolchain.toml` that differs from the one it started with, every allowlisted command
  is refused: those files can change what cargo runs whatever its arguments say.
- On macOS a sandboxed build cannot read the rest of the checkout it runs in (only its cargo
  files), so a `.env` at the checkout root stays private. Every value in that `.env` is also
  removed from what a worker's model sees, and build and test output is scrubbed of
  key-shaped strings before the model sees it.
- A sandboxed build cannot read your per-user cargo config (`~/.cargo/config.toml`,
  `~/.cargo/config`, or the one in `$CARGO_HOME`) on macOS or Linux, since it can hold a
  registry token or a proxy password. kGraf passes its build settings in for you: a source
  replacement (a mirror or vendored directory), registry index URLs, rustflags, rustdocflags,
  a linker, `target-dir` and `net.offline` are copied into a cargo home of the command's own
  (`.kgraf-cargo-home` in the worktree's scratch directory, named by `CARGO_HOME`), with your
  registry cache linked in. Nothing else is copied: no token, credential provider, `[http]`
  proxy, `[env]` value or alias. A source or registry URL with a login in it
  (`https://user:secret@host`) is not copied either, and a failing build's output names each key
  left out. Your project's own `.cargo/config.toml` still wins over these settings, as it does
  outside the sandbox. Package-index logins (pip, Yarn, Poetry) and other command-line tool
  logins in your home are unreadable too. On Linux the Poetry, Pipenv, Playwright, Cypress and
  Deno caches and environments are readable, so tests that use them run.
- The sandbox cannot download or unpack crates. A run checks that the dependencies are in
  the local cargo cache before it starts, and fetches them outside the sandbox only with the
  project's consent (see [Set up a project](#set-up-a-project)).
- `--shell-timeout` (on `worker`, `orchestrate` and `schedule run-now`) bounds one worker
  shell command, in seconds; `shell_timeout_secs` in `.kgraf/worker.json` sets it for the
  project; without either it is 600. A command killed at the timeout returns its output so
  far, marked `[killed after N s]`.
- `--resume <run-id>` continues a killed run from its record without repeating a paid call.
  With it, `--retry-gate <gate>` or `--waive-gate <gate>`, plus `--gate-note`, decides a
  held gate.
- `--interactive` reads notes and `?` status questions from stdin while the run goes.
- `--gate-timeout` (1800 seconds) bounds each gate command and each run of the `--verify`
  command. On macOS and Linux a gate command also runs under a CPU-time limit of twice
  that, so a process that escapes its process group cannot spin without limit.
- Interface contracts are experimental and off by default. `--contract-checks` turns them
  on (`contract_checks` in `.kgraf/run.json`). With them on, where one contract uses
  another's code, the planner also writes an interface contract: a stub, a reference, the
  consumer's expectations and a test. Before the first worker starts, the sandbox checks
  it: the consumer and the test compile against the stub, the test fails against the stub
  and passes against the reference, and the two parties' handshakes agree. A contract that
  fails is held, with the file that failed named. A contract whose compiler or test runner
  is not installed is held too, with the program named. A worker can challenge a contract
  with the `challenge_contract` tool; the answer names the artifact that settled it, or
  says it is judgment.
- `--check-timeout` and `--check-compile-timeout` (600 seconds each) bound one check run.
  `--planner-max-tokens` (8192) is the output limit of each orchestrator call. An answer
  cut off at that limit fails and names the flag; interface contracts carry whole files,
  so they may need more.
- One Rust interface costs about 12.5 seconds of checks and one Python interface about
  5.4 seconds on a small project. The run report says whether checks were on.

## Start from the project's own documents

```
cargo run -p kgraf-cli -- orchestrate --corpus --route kimi:<model-id> --prices prices.json
cargo run -p kgraf-cli -- orchestrate --corpus --correction "the export is CSV, not JSON" ...
cargo run -p kgraf-cli -- orchestrate --corpus --approve <plan-id> --accept-command "cargo test" ...
```

`--corpus` reads the repository's documents and code, prints the plan it read with a plan
id, and stops; nothing is created in the repository. `--correction` reads again with your
correction. `--approve <plan-id>` runs that stored plan with no new model call; a proposed
validation command becomes a gate only when you name it with `--accept-command`. In an
empty directory, give `--instruction` too. The reading's work items become presumed work
nodes at once (the tokens are the reading's, already in the ledger), except those a
roadmap node already stands for; approving the plan adopts them.

## MCP tools, stranded worktrees and scheduled runs

```
cargo run -p kgraf-cli -- allow mcp__<server>__<tool> --note "why"
cargo run -p kgraf-cli -- allow mcp__<server>__<tool> --revoke
cargo run -p kgraf-cli -- clean <worktree> [--force]
cargo run -p kgraf-cli -- schedule add nightly --instruction task.txt --zone America/New_York --cron '0 0 3 * * *'
cargo run -p kgraf-cli -- schedule add docs-check --node <work-node-id> --instruction task.txt --zone America/New_York --cron '0 0 4 * * *'
cargo run -p kgraf-cli -- schedule list --prices prices.json
cargo run -p kgraf-cli -- schedule run-now nightly --route kimi:<model-id> --prices prices.json
```

- MCP servers are configured in `.kgraf/worker.json` (section `mcp_servers`). An
  unattended worker is denied an MCP tool until `allow` records a rule in
  `.kgraf/allow-rules.json`.
- `clean` removes a worktree whose owner was on another machine or in another PID
  namespace. It refuses while the owner or any process started in it runs; `--force` is
  needed when a run record still names it.
- `schedule` also has `remove` and `enable`. `--zone` is required and an unknown zone is
  refused; give exactly one of `--cron` (seconds first) or `--at` (RFC 3339).
- A schedule is a property of a work node. `schedule add --node <id>`
  attaches it to that node. Without `--node`, a node `schedules/<id>` is made from the
  instruction. Each run is recorded on the node: start, outcome and spend by run id. The
  galaxy shows the schedule, its last run and any disable reason on the node's label.
  `remove` detaches the schedule; the node keeps its history.
- `schedule run-now` refuses a schedule the claim table has disabled unless you pass
  `--force`, and holds the schedule's run lock while it runs, so the scheduler does not
  start the same schedule beside it and a second `run-now` is refused. `--verify` defaults to
  the first test command the project's build files declare. A run that ends blocked or
  stopped makes `orchestrate` exit non-zero, and `run-now` records it as failed.

## The desktop app

```
cargo run -p kgraf-shell --bin kgraf-app -- --project /path/to/repo
cargo run -p kgraf-shell --bin kgraf-app -- --probe
```

- `--project` opens that project and shows it, its galaxy fed live from its index (run
  `kgraf index` there first). The projects you opened before open too.
- `--probe` prints the graphics adapters and exits. `--force-software` accepts only a
  software (CPU) adapter. The window always states when it renders on the CPU.
- `--screenshot FILE` captures the window to a PNG after the first frame and
  `--screenshot-after` seconds (default 3), then quits.
- Closing the window can leave the app in the tray, where scheduled runs fire.
- **Updates.** Off by default: Settings > "Check for updates on start" lets the app read
  the GitHub Releases of `Marceux-Croix/kGraf` at start, and "Check now" always checks.
  This is the only network request the setting causes. An offered release counts on the
  bell; its menu row says "kGraf <version> is available" and carries the button that
  downloads it. Accepting twice queues one download. The download is checked against the
  release's `SHA256SUMS` and refused when the line is missing or does not match. The
  checksum file comes from the same release as the installer, so it detects a corrupt
  download and does not prove who published it; a detached signature is not built yet.
- The app builds runs from `.kgraf/run.json`: the `orchestrate` flags as JSON
  (`routes`, `prices` and the optional ones, snake case; unknown keys are refused, and so is
  `worktrees`, which the app's settings switch decides). An old
  file that still sets `cap_run`, `cap_task` or `cap_hour` loads, prints one notice, and the
  caps do nothing (decision 0080). There is no settings screen for routes yet, so write
  this file by hand.
  Example: `{"routes": ["kimi:<model-id>"], "prices": "prices.json"}`.
- The settings file holds the compaction slider (5% to 95%, default 80%), the
  background-mode switch, the worker-worktree switch (`delete_worker_worktrees`, default
  on), the caps, the theme (device, dark or light), the color
  vision mode, reduced motion, whether the guide is open, and the open projects. Two
  windows share it: each saves under a lock and keeps the projects the other opened or
  closed. An unknown key is refused, so a settings file that a newer build changed does
  not open in an older build. A key left at its default is never written.
- **Appearance > Color vision** (Settings): standard, protanopia, deuteranopia,
  tritanopia or achromatopsia. Remaps the five semantic colors only, chosen so they
  stay distinct under a simulation of that color vision deficiency. It applies live, with
  no restart.

**The layout** uses a small set of meaning colors: violet (the accent) for progress and live, cyan for the
selection, a match and the primary action, yellow for needs input, orange on a failed
thing itself, gray for the rest.

- **Masthead:** the menu button, the kGraf mark with the branch, the project, the search
  field, the scheduler toggle with "n of m workers" (it only shows the cap; the cap is
  set in Settings), the render badge (GPU, or CPU rendering in the danger colors), and
  the bell. The bell counts everything that needs you over every open project:
  approvals, questions, held runs and failures; its menu jumps to the pinned card.
  A failure (a box run, a scheduled run, a gate or a check) is a pinned card with
  Dismiss. Dismissing clears it from the dot and the bell, and is kept in
  `.kgraf/acknowledged.json`; a new run of the same work supersedes the failure, and a
  failure of a resumed run is a new card. Scheduled failures come from the graph's
  history, so one from while the app was closed shows at launch.
- **Guide:** collapsed, a rail with Graph, Search, Providers and Settings (the only way
  into settings). The menu button opens the sidebar, which stays open until closed:
  Projects (switch, add a folder, close from the row's menu; a project that works cannot
  be closed; a close pauses its scheduler so no scheduled run starts in between, and
  while the scheduler finishes a check the row says "Closing"),
  Graph and Search, then Configuration. Each project row has a dot: yellow
  needs input (a failure counts as needs input), cyan new results since you looked,
  blinking gray working, none idle; a dot is never orange. The collapsed menu button
  takes the same order over all projects, and its tooltip names them.
  Runs in a project keep going while another is shown.
- **Center:** filter chips (status and crate; each is a search, so its result is a
  selection on the graph), then the graph on a near-black stage with its legend, minimap,
  selection title, the run scrubber (a placeholder at the live edge: going back in time
  is not built) and the view controls (legend, minimap, zoom, fit; keys
  `/`, `m`, `+`, `-`, `0`). Below: the project row, the pinned approval cards (a choice
  asks to confirm), and the composer with the tokens the ledger attributes to the run.
- **Right:** "Up next", the one text surface (Selection and Search tabs; transcripts as a
  chat, search results as rows with a static graph thumbnail), and "Live activity", the
  graph change log newest first, where a row selects its node.

**Inside the window.** The orchestrator box is always present. `/run <instruction>` starts
a paid run for the shown project through the same construction as `kgraf orchestrate`,
and says what is missing when `.kgraf/run.json` is absent or invalid. `/status`
answers from the run record. Tool approvals are answered in place (allow once, allow
always, deny). A run whose dependencies need fetching, in a project with no answer yet,
waits on a card (Allow once, Always for this project, Not now) and starts once allowed. Play, stop and your own order drive execution; the two concurrency caps
are in settings. The search field takes inline operators, and its result is a selection
on the graph, with snippets in the one text panel beside it. Work nodes show done, in
flight, blocked (with an edge to the cause), held by the cap, and colliding. Not built
yet: expanding a work node into its children, and `/compact`.

## Instructions and skills

```
cargo run -p kgraf-cli -- instructions scan
cargo run -p kgraf-cli -- instructions import ~/some/folder/with/skills
cargo run -p kgraf-cli -- instructions show
```

`instructions` gathers the instruction files (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, and
others) and `SKILL.md` skills your other coding agents already use into one inventory.
It reports conflicts and malformed files and never silently picks between two variants;
`instructions choose` resolves a conflict.

Workers use the skills in that inventory (`.kgraf/inventory.json`). `worker` and
`orchestrate` read it once, when the run starts; run `instructions scan` again to pick up
changes for the next run. An inventory that exists and cannot be read stops the run. A
Terminal-Bench run (`--tbench-bridge-socket`) reads no skills, and says so on stderr when
an inventory is there.

- **Listing.** Every worker's prompt lists each skill by name and description, and the
  worker gets a `skill` tool to activate one. The skill's instructions come back as that
  tool's result.
- **`disable-model-invocation: true`.** The skill is not listed, and the `skill` tool
  refuses it.
- **`allowed-tools`.** Once a worker activates such a skill, kGraf refuses every call to a
  tool outside the list. The worker sees the refusal and the skill's name as the call's
  result. The restriction lasts for the rest of that worker's run, a resumed run included,
  and applies to that worker and the subagents it spawns while restricted. Two active
  skills allow only the tools both list.
  Entries name kGraf's tools (`read_file`, `list_directory`, `search_files`,
  `apply_patch`, `shell`, `graph_slice`, `skill`, `mcp__<server>__<tool>`), with a
  trailing `*` for a prefix (`mcp__git*`) and `mcp__<server>` for all of a server's
  tools. An entry naming another agent's tool (`Read`, `Bash`) matches nothing here, so
  a skill written only for another agent refuses every call; edit its list to use these
  names.
- **Resuming after a skill changed.** A resumed worker re-activates its skills at the
  version it activated. If you edited, removed or disabled one of them since, the resume
  stops and names the skill: restore the skill as it was, or start the work again as a new
  run.
- **No skills.** With no inventory, or none a worker may invoke, workers get no `skill`
  tool and their prompt is unchanged.

The orchestrator's own planning calls use no tools, so skills apply to workers only.

## Linux sandbox: cargo files above a worktree

A worktree lies inside your checkout (`<checkout>/.kgraf/worktrees/<name>`). Cargo run
in it reads the checkout's `Cargo.toml` and `.cargo/config.toml` above it. The Linux sandbox
refuses reads outside the worktree by default, so it grants these files one at a time.
On macOS, reads use a deny list, so this section does not apply.

- **Opt-in per command.** Gates, the verifier and the worker's shell tool ask for these
  reads. Contract checks never do: their tree is a copy under `/tmp` or another scratch
  root, and another user could put a `.cargo/config.toml` above it.
- **Bounded at the checkout.** The limit is the checkout root the caller gives, never
  derived from git. Only `Cargo.toml`, `.cargo/config.toml` and `.cargo/config` are
  granted, read-only and never executable. `~/.cargo`, `$CARGO_HOME` and your own
  read-denied paths are never entered. With no home directory known, no `.cargo` file is
  read.
- **Owner and mode trust.** A file is granted only when it and every folder from it up to
  the checkout are owned by you or root and not writable by others. Group-write is
  accepted only for your user private group: your primary group, named for you, with no
  other listed member and no other account using it as its primary group. That covers the
  umask 0002 default of Debian, Ubuntu and Fedora. If the system cannot list its accounts
  (sssd or LDAP with enumeration off), group-write is refused.
- **A refusal names its cause.** Each cargo file that exists and is not granted is
  appended to a failing gate, verification or shell command as a line such as
  `[sandbox] <checkout>/Cargo.toml is not readable in the sandbox: <checkout> is
  group-writable (mode 775) by group 100, which is not this user's private group`.
  `orchestrate` also prints each one once, as a `sandbox` line, when a run opens.

To fix a refusal, remove group or other write from the named folder or file
(`chmod g-w,o-w <path>`), or give it to your own user. Other tools that read
configuration above their directory, such as pytest, get no such rule.

## kGraf's own git

kGraf runs git on your machine, outside the worker sandbox, to create worker worktrees,
commit and merge their work, read status and history, and check a Bypass worker's writes to
`.git`. A repository can make git run a program or reach the network by itself, and a worker
in Bypass may write the git config and the attribute files, so kGraf's own git ignores all of
that:

- hooks, in `.git/hooks`, in a `core.hooksPath` directory such as `.githooks`, and
  config-defined hooks (`hook.<name>.command`);
- every filter (`clean`, `smudge`, `process`), merge driver and diff driver (`textconv`,
  `command`) the config defines, whatever `.gitattributes` selects, and `diff.external`;
- the file system monitor, credential helpers, `core.askPass`, editors and pagers, commit and
  tag signing, and signature checks;
- every network or local transport (a fetch, a submodule clone, a lazy fetch in a partial
  clone), and submodule recursion;
- `GIT_*` variables in kGraf's environment, except your commit identity
  (`GIT_AUTHOR_NAME` and friends) and `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM`.

What kGraf keeps: your identity and your global config, and git's built-in attributes
(`text`, `eol`, `ident`, `core.autocrlf`), so commits and merges come out as your own git
would make them. Two differences follow. A file your attributes route through a filter, such
as git-lfs or git-crypt, is committed and checked out by kGraf in its stored form, not
filtered. A file with a configured merge driver is reported as a conflict when two attempts
change it, rather than resolved by the driver. kGraf needs git 2.31 or newer for this and
refuses to run an older one.

Your own git is unchanged: when you commit, merge or check out yourself, every hook, filter and
driver runs as before.

## What lives where

State lives in `.kgraf/` inside the repository: the index, the ledger, run records,
worktrees, schedules and allow rules. kGraf adds `/.kgraf/` to the repository's
local exclude file (the `info/exclude` that `git rev-parse --git-path info/exclude` names,
so a linked worktree works too) and never edits your tracked `.gitignore`. If the
repository already tracks files under `.kgraf/`, it warns and names the command that
untracks them; it never changes your index. It holds the ledger, the allow rules and the
run records, so never commit it. Nothing leaves the machine except calls to the providers
and MCP servers you configure, and those calls carry code from the repository.

## Coming from Haustoria

kGraf was earlier named Haustoria. What the former name left
keeps working, and each move prints one notice:

- A project's `.haustoria/` moves to `.kgraf/` on first use. Git worktree registrations,
  attempt refs and the absolute paths in run records and configuration are repaired; the
  ledger, transcripts and traces move unchanged. A project that has both directories is
  refused until you keep one. The move is also refused, before anything changes, when
  git cannot read the repository (or is missing while worktrees exist), when an older
  process still works in a worktree, or when the old directory holds a symbolic link
  where the move would write. Anything that still names `.haustoria` after the move is
  listed in the notice.
- A `HAUSTORIA_*` variable (`HAUSTORIA_KIMI_API_KEY`, `HAUSTORIA_LOG`, ...) is still read
  when its `KGRAF_*` name is not set, with a warning naming the new variable.
- An MCP OAuth token stored under the keychain service `haustoria-mcp-oauth` moves to
  `kgraf-mcp-oauth` the first time it is read.
- The desktop app's settings and log directories move from `Haustoria` to `kGraf` on its
  first launch. On Windows, the kGraf MSI upgrades an installed Haustoria; on Debian,
  the `kgraf` package replaces `haustoria`.
- A worker transcript recorded before the rename does not resume: the worker's system
  prompt names the product, so the prefix differs and the runtime refuses, saying the
  transcript predates the rename. Finish or stop running work before you upgrade.

## Changes users will notice

Behavior that differs from earlier builds. Each item names the decision that holds the reasons.

**Cost and spend**

- A model with no price entry runs, and its spend is shown as unpriced (a local model as
  local, unpriced), never as zero dollars (0075).
- A model priced from the bundled catalog is priced only up to the input size where its vendor
  changes rates. A larger call is shown as unpriced, not costed at the base rate. A price
  entry you set yourself replaces the catalog's tier for that model (0075).
- `kgraf spend` prints `total unpriced` when no recorded model has a price, and
  `total <amount> (partial: some calls could not be costed)` when some calls could not be
  costed (0075).
- Money is summed at full precision and rounded once, so a total can differ by a few
  micro-dollars from a sum of separately rounded calls (0075).
- A benchmark attempt reads as unpriced only when the price table cannot cost its calls. A
  ledger read error stops the attempt with the error.

**The ledger**

- A settle is accepted only for a call that is in flight, and a reconcile only for a call that
  is unreconciled or in flight. Anything else is refused with the call's state, so a resolve
  cannot overwrite a counted row and a late settle cannot overwrite a reconciled figure.
- `kgraf unreconciled` lists each open call with its state (`in_flight` or `unreconciled`) and
  the usage recorded so far, which is a lower bound. A call still in flight needs
  `--abandoned` before `resolve` will close it.
- `kgraf unreconciled resolve` keeps the recorded figure for any field you leave out. Only
  `--no-charge` states zero for every field.
- When an Anthropic-protocol endpoint sends a usage object that does not add up (a cached
  count above the cache read, or a total that differs from the parts), the reply is returned,
  the call stays unreconciled with the counted fields as a lower bound and a note in the raw
  usage, and a warning is logged. Resolve it as any other open call (0006).

**Runs**

- A contract waiting on a route that is not ready is held after 2,400 checks from the first
  failure (20 minutes at the default tick). The hold says how long it waited and how to try
  again: fix the route, then `kgraf orchestrate --resume <run>`. Earlier builds waited without
  end (0075).
- A contract whose route is not ready moves the rotation on once for each route it finds not
  ready, not once for each check (0075).
- A file made only of plain structs and trait impls whose bodies are all `todo!()` or
  `unimplemented!()` counts as an abandoned stub. An enum, a constant, a macro call, an
  inherent impl, anything under `cfg`, or a struct with a non-standard derive marks the file as
  finished code (0086).
- A background process that leaves a worker's process group keeps the worktree from being
  deleted. The worktree stays, with a warning; stop the process, then run `kgraf clean
  <worktree>` (0088).
- A worker at the end of a chain of hand-backs (the delegation depth cap) no longer stops its
  contract when its context fills. kGraf compacts that worker once and it goes on: one
  summary call on the worker's own route, recorded in the ledger as `worker_compaction`; the
  worker's instruction is shown to it again unchanged; its earlier history stays in its
  transcript, and the worker can search it with the `recall_history` tool. The run report
  counts the compaction with the hand-backs (`misses`), the trace has a `worker_compacted`
  record naming the transcript file, and the run record lists it under `compactions`. A
  worker that fills its context a second time hands back, and its contract is held with a
  reason that names the worktree and the transcript (0095).
- Every worker's tool list now includes `recall_history`, so a worker transcript recorded by
  an earlier build cannot be resumed; start that work again (0095).
- kGraf's own git runs no hook, filter, merge driver or diff driver from your repository or
  config, and reaches no remote (see "kGraf's own git"). A file stored through git-lfs or
  git-crypt is committed by kGraf in its stored form, and a file with a configured merge
  driver conflicts when two attempts change it. git 2.31 or newer is required (0094).
- In Bypass, workers may write `.gitattributes`, `.gitmodules` and `.githooks` when the
  contract declares them; they merge as ordinary files (0094).

**Scheduled runs**

- A manual run (`kgraf schedule run-now`) keeps its lock when the machine slept past the lease
  and nothing else took the schedule. The run is reported as failed only when another manual
  run or a scheduled claim took the lock.

## Installers

Local packaging scripts build a platform-native artifact from this tree:

| Script | Artifact | Signing |
|---|---|---|
| `scripts/package-macos.sh` | universal (Apple silicon and Intel) `kGraf.app` and `.dmg` | ad-hoc only; notarization runs when a Developer ID exists |
| `scripts/package-linux.sh` | tarball and `.deb` | none |
| `scripts/package-windows.sh` | x64 `.msi` (built with `wixl`) and `.zip`, cross-built with `cargo-xwin` | none |

Each package carries `LICENSE` and `THIRD_PARTY_NOTICES.md`. The version comes from one
place, `[workspace.package].version` in the root `Cargo.toml`, and
`scripts/check-release-version.sh` fails when it does not match the highest `v*` tag's
number. The packages are not signed with a developer certificate yet, so expect Gatekeeper
and SmartScreen warnings. `scripts/release-all.sh` builds every installer from one git
tag. A `cargo-dist` workflow in `.github/workflows/release.yml` builds draft releases from a
version tag.

## Diagnostics

kGraf keeps one small, local, per-user file of basic usage counts (launches,
runs started and finished, workers spawned, the providers and models used by
name, and named feature use) and every error, each under a stable `KG-nnnn`
glossary code (`docs/ERRORS.md`). It is capped at about 1 MB, with the oldest
entries trimmed once it grows past that. **Nothing in it is ever sent
anywhere**: there is no network code in the crate that writes it at all. If
you ever need to report a problem, run `kgraf diag show` to read it, or `kgraf
diag export` to write a shareable copy, and send that however you choose.

