# 附录 A：JSON-RPC 接口清单

本附录列出 `drsg serve` 暴露的 JSON-RPC 2.0 方法。位于
`crates/dr-strange-web/openrpc.json` 的 `OpenRPC` schema 是权威来源——服务端从
`rpc.discover` 返回它，各 SDK 由它生成。每个方法都带有一个**访问**层级
（`read`、`write` 或 `admin`）；在单一共享令牌下，三者需要同一个令牌。

## 发现与数据库

| 方法 | 访问 | 摘要 |
|---|---|---|
| `rpc.discover` | read | OpenRPC 服务描述 |
| `db.stats` | read | 平面/节点/边计数、标签、边类型、索引、提交数、磁盘占用 |
| `db.catalog` | read | 跨所有平面汇总的软 schema 目录 |

## 平面

| 方法 | 访问 | 摘要 |
|---|---|---|
| `plane.list` | read | 每个平面及其 id、名称、计数与属性 |
| `plane.catalog` | read | 单个平面的软 schema |
| `plane.indexes` | read | 某个平面上已声明的检索索引 |
| `plane.history` | read | 时间旅行窗口（仅原生后端） |
| `plane.create` | admin | 创建一个空平面 |
| `plane.rename` | admin | 重命名一个平面 |
| `plane.set_props` | admin | 替换一个平面的属性映射 |
| `plane.delete` | admin | 删除一个平面及其内容 |

## 节点与边

| 方法 | 访问 | 摘要 |
|---|---|---|
| `node.get` | read | 按 id 或外部键获取一个节点 |
| `node.create` | write | 新增一个节点（可选键与标签） |
| `node.update` | write | 修补一个节点的属性与标签 |
| `node.delete` | write | 删除一个节点，并级联其边 |
| `edge.create` | write | 在两个节点间新增一条有向边 |
| `edge.update` | write | 修补一条边的属性或类型 |
| `edge.delete` | write | 删除一条边 |

## 查询与检索

| 方法 | 访问 | 摘要 |
|---|---|---|
| `plane.neighbors` | read | 一跳扩展，以 `{node, edge}` id 对返回 |
| `plane.query` | read | 运行一个序列化的逻辑计划 |
| `plane.cypher` | write | 运行一条 openCypher 子集语句（写门控） |
| `plane.find` | read | 对一个平面的文本或语义搜索 |
| `plane.search` | read | 在某个属性上的向量 top-*k* |
| `plane.hybrid` | read | 融合的向量 + 关键词 + 图邻近度检索 |
| `plane.algo` | read | 一个图算法（pagerank / components / shortest_path / louvain） |
| `plane.ask` | read | 自然语言查询 → 计划 → 执行 |
| `graph.seed` | read | 一块由若干节点及其诱导边构成的初始画布 |
| `graph.expand` | read | 围绕某个节点的、防枢纽（hub-safe）的一跳邻域 |

`plane.query`、`plane.neighbors`、`plane.find`、`graph.seed` 与 `graph.expand`
接受可选的 `as_of`（提交序号）或 `as_of_ms`（时间戳）参数，用于时间旅行读取
（仅原生后端）。

## 索引与导入

| 方法 | 访问 | 摘要 |
|---|---|---|
| `index.ensure` | admin | 在 `(标签, 属性)` 上声明一个向量或关键词索引 |
| `digest.run` | write | 经由 LLM 从文本抽取节点/边方案（dry-run） |
| `digest.write` | write | 写入一个先前算得的方案（不调用 LLM） |

## WebSocket 订阅

`/ws` 端点应答上述相同的请求/响应方法，并额外支持变更流（以下为 WebSocket 专有）：

| 消息 | 方向 | 摘要 |
|---|---|---|
| `plane.watch` | 客户端 → 服务端 | 订阅某个平面的变更（可选 `label`） |
| `plane.unwatch` | 客户端 → 服务端 | 停止订阅 |
| `plane.change` | 服务端 → 客户端 | 一个已提交的变更集 `{plane, seq, truncated, changes}` |

## 错误码

| 码 | 含义 |
|---|---|
| `-32700` | 解析错误 |
| `-32600` | 无效请求 |
| `-32601` | 方法未找到 |
| `-32602` | 无效参数 |
| `-32000` | 应用错误（错误平面、悬挂端点、冲突……） |
| `-32001` | 未授权（凭据缺失或无效） |
