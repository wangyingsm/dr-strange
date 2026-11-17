# Ops graph

**Status**: begun 2026-10-17 as design. Shipped since: the `<name>_ops` plane
(§2), the Compose, Kubernetes, Terraform and Prometheus readers, the image
join and the `deployed` verb over it (§5, §9), and the observed `CALLS` edge
a trace export folds into (§4, §6). Still design: cluster runtime (`Pod`,
`Node`), deploy events and incidents (§8).

The year's thesis is that a code commit and a production incident are two
ends of one graph. This doc fixes the half that does not exist yet: what an
operations topology looks like as nodes and edges, how it is identified, and
— the part that decides whether the thesis holds at all — how it joins to the
code and history planes that are already there.

Four decisions are settled, and the rest of this doc is their consequences:

1. Ops data lives in **its own plane**, `<name>_ops`, the way history lives
   in `<name>_git` — and a digest writes it **without being asked to**.
2. The **container image** is the join carrier between a running thing and
   the commit that built it.
3. The plugins read **declared state** — manifests as written — and not a
   live cluster.
4. ConfigMap **values are recorded**; Secret values never are.

## 1. Scope: declared state, not runtime

In scope for the first version, because a file in a repository is all a
parser plugin needs:

| | |
|---|---|
| Kubernetes | `Deployment`, `StatefulSet`, `DaemonSet`, `Job`, `CronJob`, `Service`, `Ingress`, `ConfigMap`, `Secret`, `PersistentVolumeClaim`, `Namespace` |
| Compose | services, networks, volumes, `depends_on`, published ports |
| Terraform | `resource`, `data`, `module`, `variable`, `output`, `provider`, and the references between them |

Out of scope, and deliberately so: **Pods, nodes, metrics, alerts**. Each is
cluster state, not a declaration — it needs credentials, it is stale the
moment it is read, and none of it belongs to a plugin whose input is a
directory. The vocabulary below reserves labels for them so that Q1's
Prometheus and OpenTelemetry work extends this model instead of replacing it.

This is the same line the `deps` plugin draws: what a manifest *declares* is
worth recording even where the thing it declares cannot be observed.

## 2. The plane

A digested repository already produces two planes — `<name>` for the code as
it is now, `<name>_git` for its history. Ops is the third:

```
myrepo        code as it is now
myrepo_git    commits, branches, tags
myrepo_ops    services, workloads, config, routes
```

Its own plane because an ops resync and a code digest have unrelated
lifecycles: a chart changes on a different cadence from the code, and
re-reading one must not invalidate the other. Planes are cheap
([09-planes.md](09-planes.md) §1), so a third is not a cost.

**No flag asks for it.** An ops file in a digested tree is read because it is
there, and the reader that claimed it decides the plane — the mechanism
`preprocess/repo.rs` already uses for history, where writer and readers share
one constant rather than each keeping a copy. The exception is Kubernetes: a
manifest may be called anything and that reader claims no extension on
purpose, so a manifest tree is still digested with `--handler k8s` until
something sniffs a document for `apiVersion` and `kind`.

**And because planes are hard namespaces, this choice has a consequence that
shapes everything in §5: a cross-plane edge is rejected at the storage
layer** (`Error::PlaneMismatch`). There is no edge from a Workload to the
Commit that built it. The join is a shared property and a query, never an
edge, until the `portals` concept in 09-planes §6 either ships or does not.

## 3. Nodes

Populated by the first version:

| Label | Key | What it is |
|---|---|---|
| `Namespace` | `<name>` | A k8s namespace, or a Compose project |
| `Workload` | `<ns>/<kind>/<name>` | Anything that runs a container. `kind` on the node (`Deployment`, `CronJob`, a Compose `service`) rather than one label per kind, so a query can ask for every workload at once |
| `Container` | `<workload key>/<container name>` | One container spec inside a workload |
| `Image` | the reference as written | The join carrier — see §5 |
| `Service` | `<ns>/Service/<name>` | A stable address in front of workloads |
| `Ingress` | `<ns>/Ingress/<name>` | External entry, with host and path on the edges it makes |
| `ConfigMap`, `Secret` | `<ns>/<kind>/<name>` | Configuration. §7 governs their contents |
| `Volume` | `<ns>/Volume/<name>` | A PVC, or a Compose volume |
| `Network` | `<ns>/Network/<name>` | A Compose network. k8s has no analogue worth a node |

Terraform declares the layer the other two run *on*, and adds:

| Label | Key | What it is |
|---|---|---|
| `Infra` | `<type>.<name>` | A managed resource, keyed by Terraform's own address — `aws_instance.web`. The provider's type is a property, because there are thousands of them and a label per type would be a vocabulary nobody could query |
| `DataSource` | `data.<type>.<name>` | Something read rather than managed |
| `Module` | `module.<name>` | A module block, with its `source` |
| `Variable` | `var.<name>` | An input. §7 governs its default |
| `Output` | `output.<name>` | What the module publishes |
| `Provider` | `provider.<name>` | A configured provider |

`Infra` rather than `Resource` **on purpose**: the Kubernetes reader already
uses `Resource` as its catch-all for a kind it does not model, and one label
meaning both "an unmodelled Kubernetes object" and "a managed cloud resource"
would make every query over either one wrong.

The Prometheus reader adds four, which were reserved here before it was
written (`Metric` and `Alert`) or added when it was:

| Label | Key | What it is |
|---|---|---|
| `ScrapeJob` | `<config>/job/<name>` | One `scrape_configs` entry. `<config>` is the directory holding the file, the rule the Compose reader uses for projects |
| `ScrapeTarget` | `<config>/job/<name>/target/<address>` | One declared address. A node rather than a property on the job, because it carries the join to the service — and, where nothing answers to it, the fact that there was none |
| `Alert` | `<config>/alert/<group>/<name>` | One alerting rule, with its expression as written |
| `Metric` | `metric/<name>` | A metric a rule names. Keyed globally, not per configuration: a metric name means the same thing wherever it is written, so two configurations watching one metric meet on it |

`ScrapeTarget` and `ScrapeJob` were not in the reserved list, and `SCRAPES`
was reserved as `Metric → Workload`. That direction was written before the
input was: which metrics a job yields is runtime, discoverable only by
scraping, whereas what a job *declares* it scrapes is in the file. The edge
therefore runs from the target, and the reserved `Metric` label keeps its
meaning — what an alert watches.

A trace export adds no label of its own. A service the spans name is the same
kind of thing a manifest declares, so it is a `Service` — keyed as the
topology keys it where the topology declares it, and under `observed/<name>`
where nothing does:

| Label | Key | What it is |
|---|---|---|
| `Service` | `observed/<service.name>` | A service seen in traces that no file in this tree declares. `origin: observed` on the node says which it is, and the prefix is a namespace: a name the topology does not hold cannot collide with one it does. Asking the plane for these is asking *what is running here that nothing declares* |

Reserved, so that later work does not rename these:

`Pod`, `Node` (cluster runtime, Q1) · `DeployEvent` (a rollout, Q1 — *not*
called `Deployment`, which is a workload kind here and would collide) ·
`Incident` (Q2).

## 4. Edges

| Edge | From → to | Carries |
|---|---|---|
| `IN_NAMESPACE` | anything → `Namespace` | — |
| `RUNS` | `Workload` → `Container` | — |
| `USES_IMAGE` | `Container` → `Image` | — |
| `SELECTS` | `Service` → `Workload` | the selector as written, and `matched_by` (`selector`, `name`) |
| `ROUTES_TO` | `Ingress` → `Service` | `host`, `path`, `port` |
| `MOUNTS` | `Container` → `ConfigMap` \| `Secret` \| `Volume` | `as` (`env`, `envFrom`, `file`), `key` where one key is named, `path` for a file |
| `DEPENDS_ON` | `Workload` → `Workload` | Compose `depends_on`, with `condition` |
| `ATTACHED_TO` | `Workload` \| `Service` → `Network` | — |

Terraform's edges come from *references*, which is where its graph lives:

| Edge | From → to | Carries |
|---|---|---|
| `DEPENDS_ON` | `Infra` → `Infra` \| `DataSource` | `via` (`reference` or `depends_on`) — an interpolated address is a dependency as surely as a declared one |
| `USES_VARIABLE` | `Infra` \| `Module` → `Variable` | — |
| `USES_MODULE` | `Namespace` → `Module` | — |
| `PROVIDED_BY` | `Infra` → `Provider` | — |
| `EXPOSES` | `Output` → `Infra` | — |
| `USES_IMAGE` | `Infra` → `Image` | the same edge the other two readers draw, so a task definition reaches a revision |

`USES_IMAGE` from an `Infra` node is the join paying off twice: a container
image named in Terraform — an ECS task definition, a Cloud Run service, a
`docker_container` — reaches the same `Image` node the Kubernetes reader
would mint, under the same key, with the same `revision` parsed by the same
SDK code.

Prometheus draws:

| Edge | From → to | Carries |
|---|---|---|
| `HAS_TARGET` | `ScrapeJob` → `ScrapeTarget` | — |
| `SCRAPES` | `ScrapeTarget` → `Service` \| `Workload` | `matched_by` and `confidence` — see §6 |
| `ALERTS_ON` | `Alert` → `Metric` \| `ScrapeJob` | — |
| `DERIVES_FROM` | `Metric` → `Metric` | A recording rule's metric, to the metrics it is computed over |

A trace export draws one edge, and it is the only one here that nothing
declared:

| Edge | From → to | Carries |
|---|---|---|
| `CALLS` | `Service` → `Service` | `origin: observed`, `calls` and `errors` (the spans behind it, a count of the sample and not of the traffic), `first_seen` / `last_seen` / `last_seen_ts` — see §6 |

Reserved: `SCHEDULED_ON` (Pod → Node) · `DEPLOYED` (DeployEvent → Workload) ·
`CAUSED_BY` (Incident → DeployEvent).

`SELECTS` is the one edge with real work behind it. A k8s Service names pods
by label selector, and the pods come from a workload's pod template — so the
edge is computed by matching the Service's `spec.selector` against each
workload's `spec.template.metadata.labels`. Where a selector matches nothing
in the tree, the Service is recorded and the edge is not, with the selector
kept on the node so a reader can see what it asked for.

## 5. Identity, and the image join

Within the ops plane, keys are structural and exact: a namespace, a kind, a
name. Nothing is guessed.

Across planes, the carrier is the **image reference**, because it is the one
string both a manifest and a build pipeline name. The `Image` node keeps the
reference as written, plus whatever is parseable from it:

```
ghcr.io/acme/api:sha-9f2c1b7
└─ registry ──┘ └─repo─┘ └── tag ──┘   → revision = "9f2c1b7"
```

A tag component that looks like a git revision — `sha-<hex>`, `git-<hex>`, a
bare 7-to-40 character hex string — is recorded as a `revision` property. A
digest (`@sha256:…`) is recorded as `digest`, which identifies the image
exactly but says nothing about the commit.

**The join is then a query, not an edge**, and it runs across two planes:
read `revision` from `myrepo_ops`, look up that commit in `myrepo_git`. Both
halves are exact; the only inference is that the tag means what it looks
like.

## 6. Where the join holds, and where it does not

- **A revision-tagged image joins cleanly.** `api:sha-9f2c1b7` and commit
  `9f2c1b7` are the same thing by construction, because a pipeline put the
  sha in the tag.
- **A floating tag joins to nothing.** `api:latest`, `api:v2`, `api:stable`
  name no commit. The image is recorded; `revision` is absent, and a query
  that wants a commit gets nothing rather than a guess.
- **A digest-only reference is exact but silent.** `api@sha256:…` pins the
  bytes and omits the provenance. Resolving it needs a registry call —
  outside what a parser does, and a candidate for Q1's deploy-event work.
- **A third-party image never joins, correctly.** `postgres:17` is not built
  from this repository, and nothing should connect it to a commit here.
- **The repository may not be the one that built the image.** A monorepo of
  charts deploys images from elsewhere. The `revision` is recorded either
  way; whether `myrepo_git` holds that commit is a question the query
  answers, not an assumption the model makes.

The rule, same as `deps`: record what the manifest says, join only where the
join is exact, and state the gap where it is not.

### A scrape target, and the service it scrapes

The same rule meets its second case with Prometheus, where it cannot be kept
whole: a scrape target is an *address*, and only one address form is also a
key. `api.prod.svc.cluster.local` is `prod/Service/api` and admits no other
reading. A bare `api:9090` — a Compose service, a short-name Kubernetes
target — names no namespace, and the reader holding one configuration file
cannot invent one.

So the join is split in two, and the halves are never written to look alike:

- **The reader states, the host joins.** A target records `names` where its
  address is a key. Whether this tree *declares* that service is not a
  parser's question — and an edge onto a key nothing mints fails the whole
  digest — so the host draws the edge, as `matched_by: dns`,
  `confidence: exact`, or leaves the target unmatched.
- **A bare host is matched by name, and said to be.** The host sees every ops
  node in the plane, so it can match `api` against what the Compose and
  Kubernetes readers minted. One match is an edge marked `matched_by: name`,
  `confidence: low`. It is a weaker claim than an address, and a reader must
  be able to tell which it has.
- **Two matches are candidates, not a choice.** Where `api` names a Compose
  service *and* a Kubernetes Service, no edge is drawn and both keys are
  recorded on the target. The ambiguity is the answer.

This is `wire`'s shape — a correspondence neither side declares, computed
where both sides are visible — except that it is computed once at write time
rather than per question, because an ops plane is small and a target's
address does not change between questions.

### A traced service, and the service it is declared as

The third case comes from outside the tree. A span carries a `service.name`
and nothing else about the topology: no namespace, no file, no key. So a
traced service is matched by the same lookup the bare scrape host uses — one
match is the key to draw on, several keep the service observed with the
candidates recorded — and the rest of the arrangement is what the direction
of the input forces:

- **A span is an input, not a node.** A trace export is a sample of requests,
  and 10⁵ spans would be 10⁵ nodes nobody queries. What is written is the
  dependency they witness, with the count of the spans that witnessed it, and
  the edge says in its own property description that the count is of the
  sample rather than of the traffic.
- **A declared service is not rewritten.** The edge is drawn onto the key the
  k8s or Compose reader minted, and nothing is added to that node. Writing
  `last_seen` onto it would also overwrite `_generated_by`, and the plane
  would then name the wrong reader for everything on it.
- **The fold's own nodes are not a declaration.** `observed/<name>` answers to
  the same name as the service it stands for, so the second import would
  match the first import's node and count it as declared. Keys under
  `observed/` are excluded from the lookup for that reason.
- **A span whose parent is missing is counted and reported.** Sampling drops
  spans and the file exporter flushes per batch, so a child's parent is
  routinely in a line, a file or a window this read does not hold. Those spans
  are counted and the edge set is stated as a lower bound by exactly that many
  — the same note `affected` carries, for the same reason.

Where it comes from is the host's business, not a parser's: §1 keeps runtime
state out of a plugin because it needs credentials and is stale when read.
`drsg ops traces --from <export>` reads a file today; a pull from a trace
store is the same fold behind a different reader.

## 7. Config and secrets

**ConfigMap values are recorded.** A query can then see what a service is
configured with, which is most of what a config question asks.

The consequence has to be stated rather than discovered: those values land
in the plane, and therefore in every backup and export of it. A ConfigMap in
a real repository sometimes carries things that should have been a Secret —
a connection string with a password in it is the usual one. **A plane built
from a repository's manifests should be treated as carrying whatever those
manifests carry.**

**Terraform states it outright, and that is the rule.** A `variable` or
`output` marked `sensitive = true` is the author saying so, exactly as
choosing `ConfigMap` over `Secret` is — so a default or a value is recorded
unless the block declares itself sensitive.

Two Terraform files are **never read at all**: `*.tfvars` is values rather
than declarations and is where real credentials are kept, and
`terraform.tfstate` is runtime state that contains every attribute of every
resource in plain text, including the secret ones. Neither is a declaration,
so neither is in scope under §1 even before the secret question arises.

**Secret values are never recorded**, in any encoding. The node carries the
name, the keys, and the type; `data` and `stringData` values are dropped at
parse time, not redacted later. `base64` is not encryption, and a plugin that
read it would put real credentials in a graph that gets copied around.

## 8. What the later quarters add

The model is shaped so these extend rather than rewrite it:

- **Q1 — Prometheus**: **shipped**. `ScrapeJob`, `ScrapeTarget`, `Alert` and
  `Metric` nodes from the `prometheus` plugin; `SCRAPES` to the service a
  target resolves to, drawn by the host (§6); `ALERTS_ON` from an alerting
  rule. `prometheus.yml` is routed by name like a Compose file, and rule
  files are followed from the `rule_files:` the configuration declares, since
  they have no name a router could claim.
- **Q1 — OpenTelemetry**: **shipped in part**. `CALLS` between services, folded
  from an OTLP/JSON export by `drsg ops traces` (§6), carrying
  `origin: observed` and the spans behind it — the first edge in this plane
  that nothing declared. Still to come: the pull from a trace store, so the
  input is a query over a window rather than a file, and the plugin that reads
  an export committed to the tree.
- **Q1 — deploy events**: `DeployEvent` nodes joined to the image digest and
  so, through §5, to a commit. This is where the image-to-commit join stops
  being a query and starts being recorded.
- **Q2 — incidents**: `Incident`, `CAUSED_BY`, and the time-travel queries
  that read the topology as it was at the moment of a failure.

## 9. Open questions

1. ~~**Cross-plane reads.**~~ **Answered, and shipped**: `deployed` opens both
   planes and joins in itself — `dr-strange-core/src/ops.rs`, with the MCP
   tool and the CLI arm over it. Not `portals`, which stays unshipped and
   unneeded until a *query language* need appears. No storage or query change
   was involved: `rev.rs` already resolved an abbreviated sha and already
   stated an ambiguity rather than guessing. The reverse direction scans the
   ops plane's `Image` nodes rather than indexing them, which is nothing at
   the size an ops plane is.
2. **Helm and Kustomize.** The plugins parse rendered YAML; something has to
   render it. A `helm template` invocation is not a parser's job, so this is
   likely a CLI step that writes a rendered tree the plugin then reads.
3. **One plane per environment.** `prod` and `staging` are different
   topologies from the same chart. A plane each, a namespace node each, or a
   property on every node — unresolved, and it interacts with question 1.
4. **Whether `Container` earns its node.** It could be properties on the
   workload. It exists here because `MOUNTS` and `USES_IMAGE` are per
   container, and a sidecar is a different thing from its main container.
