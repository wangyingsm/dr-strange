# 附录 A：JSON-RPC 接口清单

本附录是 `drsg serve` 所暴露 JSON-RPC 2.0 方法的参考。位于
`crates/dr-strange-web/openrpc.json` 的 `OpenRPC` schema 是权威来源：服务端会从
`rpc.discover` 返回它，各 SDK 也由它生成。

每一条目都列出方法名、**访问**层级（`read` / `write` / `admin`；在单一共享令牌下，
三者用的是同一个令牌）、一行摘要及其参数。参数写作 `name` 类型，类型一律是 JSON
值；**`!` 标记必填参数**。`Properties` 是属性映射的方言（`{"$vector":[…]}`、
`{"$desc":…,"$value":…}`），`NodeRef` 则指一个节点 id 或一个外部键。

## 发现与数据库

- **`rpc.discover`** · read —— OpenRPC 服务描述。参数：无。
- **`db.stats`** · read —— 平面/节点/边计数、标签、边类型、索引、提交数、磁盘占用、内存占用（进程常驻集，以及已加载插件持有的字节数）。参数：无。
- **`db.catalog`** · read —— 跨所有平面的软 schema 目录。参数：无。

## 平面

- **`plane.list`** · read —— 每个平面及其 id、名称、计数、属性。参数：无。
- **`plane.catalog`** · read —— 单个平面的软 schema。参数：`plane` string!。
- **`plane.indexes`** · read —— 某个平面上已声明的检索索引。参数：`plane` string!。
- **`plane.history`** · read —— 时间旅行窗口（仅原生后端）。参数：无。
- **`plane.create`** · admin —— 创建一个空平面。参数：`name` string!, `properties` Properties。
- **`plane.rename`** · admin —— 重命名一个平面。参数：`plane` string!, `to` string!。
- **`plane.set_props`** · admin —— 替换一个平面的属性映射。参数：`plane` string!, `properties` Properties!。
- **`plane.delete`** · admin —— 删除一个平面及其内容。参数：`plane` string!。

## 节点与边

- **`node.get`** · read —— 按 id 或外部键获取一个节点。参数：`plane` string!, `id` integer, `key` string。
- **`node.create`** · write —— 新增一个节点（可选键与标签）。参数：`plane` string!, `key` string, `labels` array, `properties` Properties。
- **`node.update`** · write —— 修补属性（`set`/`unset`）与标签。参数：`plane` string!, `id` integer, `key` string, `set` Properties, `unset` array, `labels` array。
- **`node.delete`** · write —— 删除一个节点，并级联其边。参数：`plane` string!, `id` integer, `key` string。
- **`edge.create`** · write —— 在两个节点间新增一条有向边。参数：`plane` string!, `src` NodeRef!, `dst` NodeRef!, `type` string!, `properties` Properties。
- **`edge.update`** · write —— 修补属性（`set`/`unset`）或类型。参数：`plane` string!, `edge` integer!, `set` Properties, `unset` array, `type` string。
- **`edge.delete`** · write —— 删除一条边。参数：`plane` string!, `edge` integer!。

## 查询与检索

- **`plane.neighbors`** · read —— 一跳扩展，以 `{node, edge}` id 对返回。参数：`plane` string!, `id` integer!, `direction` string, `type` string, `as_of` integer, `as_of_ms` integer。
- **`plane.query`** · read —— 运行一个序列化的逻辑计划。参数：`plane` string!, `plan` object!, `as_of` integer, `as_of_ms` integer。
- **`plane.cypher`** · write —— 运行一条 openCypher 子集语句（写门控）。参数：`plane` string!, `query` string!, `embed` string, `params` object。
- **`plane.find`** · read —— 对一个平面的文本或语义搜索。参数：`plane` string!, `q` string!, `limit` integer, `semantic` boolean, `provider` string, `embed_model` string, `as_of` integer, `as_of_ms` integer。
- **`plane.search`** · read —— 在某个属性上的向量 top-*k*。参数：`plane` string!, `property` string!, `query` array!, `label` string, `k` integer, `metric` string。
- **`plane.hybrid`** · read —— 融合的向量 + 关键词 + 图邻近度检索。参数：`plane` string!, `q` string!, `label` string, `vector_prop` string, `keyword_prop` string, `metric` string, `graph_hops` integer, `graph_decay` number, `w_vector` number, `w_keyword` number, `w_graph` number, `k` integer, `candidates` integer, `provider` string, `embed_model` string。
- **`plane.algo`** · read —— 作用于一个平面或某个标签子集的图算法。参数：`plane` string!, `algo` string!, `label` string, `limit` integer, `damping` number, `max_iters` integer, `tolerance` number, `src` integer, `dst` integer, `dir` string, `weight` string, `max_levels` integer, `min_gain` number。
- **`plane.ask`** · read —— 自然语言查询 → 计划 → 执行。参数：`plane` string!, `question` string!, `dry_run` boolean, `max_attempts` integer, `limit` integer, `provider` string, `model` string, `embed_provider` string, `embed_model` string。
- **`graph.seed`** · read —— 一块由若干节点及其诱导边构成的初始画布。参数：`plane` string!, `label` string, `limit` integer, `order` string（`scan` \| `degree` \| `pagerank`，默认 `scan`）, `as_of` integer, `as_of_ms` integer。指定排序时，返回的是得分最高的节点而非扫描时最先遇到的那些，并附带其 `scores`。若要取骨架，建议用 `degree`：PageRank 会把权重汇聚到汇点，不适合这个用途。
- **`graph.expand`** · read —— 围绕某个节点的、防枢纽的一跳邻域。参数：`plane` string!, `id` integer!, `direction` string, `type` string, `limit` integer, `as_of` integer, `as_of_ms` integer。

## 索引与导入

- **`index.ensure`** · admin —— 在 `(标签, 属性)` 上声明一个向量或关键词索引。参数：`plane` string!, `label` string!, `property` string!, `kind` string, `metric` string, `language` string。
- **`digest.run`** · write —— 经由 LLM 从文本抽取节点/边方案（dry-run）。参数：`plane` string!, `text` string!, `chat` string, `embed` string, `model` string, `embed_model` string, `source` string, `no_embed` boolean, `link` boolean, `concurrency` integer, `chunk_chars` integer, `mode` string（`coarse` \| `fine` \| `super`，默认 `fine`——见[第 3 章](./ai-native.md#抽取精度)；`super` 的输入 token 用量约为 15 倍）。
- **`digest.write`** · write —— 写入一个先前算得的方案（不调用 LLM）。参数：`plane` string!, `nodes` array!, `edges` array。
- **`plane.vectorize`** · write —— 为平面中的每个节点生成向量嵌入（文本未变的节点会跳过），并按标签确保 `embedding` 上的向量索引。提供方密钥来自服务端环境。参数：`plane` string!, `embed` string, `embed_model` string, `metric` string。

## 插件

- **`plugin.list`** · read —— 已安装的预处理插件，与 `drsg plugin list --json` 输出同一种记录。参数：无。
- **`plugin.catalog`** · read —— 官方目录，读取自 extensions 仓库的 `catalog.json` 而非编译进二进制，因此插件发布不需要 drsg 发布。返回 `{stale, schema, source, plugins}`；每个条目带有 `name`、`version`、`claims`、`url`、`sha256` 与 `compat`（`ok`、`needs_host`、`other_contract`），可与 `plugin.list` 关联比对。缓存一小时；`stale: true` 表示抓取失败，返回的是服务端保留的最后一份副本。参数：无。
- **`plugin.install`** · write —— 从 `http(s)` URL 下载、校验、固定哈希并存入插件（RPC 上拒绝服务器本地路径）。参数：`url` string!。
- **`plugin.remove`** · write —— 按名称卸载插件。参数：`name` string!。

## WebSocket 订阅

`/ws` 端点应答上述相同的请求/响应方法，并额外支持变更流（以下为 WebSocket 专有）：

- **`plane.watch`** · 客户端 → 服务端 —— 订阅某个平面的变更。参数：`plane` string!, `label` string。
- **`plane.unwatch`** · 客户端 → 服务端 —— 停止订阅。参数：无。
- **`plane.change`** · 服务端 → 客户端 —— 一个已提交的变更集。字段：`plane`, `seq`, `truncated`, `changes`。

## 错误码

| 码 | 含义 |
|---|---|
| `-32700` | 解析错误 |
| `-32600` | 无效请求 |
| `-32601` | 方法未找到 |
| `-32602` | 无效参数 |
| `-32000` | 应用错误（错误平面、悬挂端点、冲突……） |
| `-32001` | 未授权（凭据缺失或无效） |
