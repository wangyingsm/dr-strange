<p align="center">
  <img src="crates/dr-strange-web/frontend/public/magic-circle.svg" alt="Dr Strange" width="120" height="120">
</p>

<h1 align="center">Dr Strange</h1>

<p align="center"><em>一个 AI 原生的嵌入式图数据库，使用 Rust 编写。</em></p>

<p align="center"><a href="README.md">English</a> · <strong>简体中文</strong></p>

📖 **Dr Strange 手册** —— 完整教程与指南：
[English](docs/en/src/introduction.md) · [中文](docs/zh/src/introduction.md)。

## 简介

Dr Strange 是一个从设计之初便面向 AI 工作负载的图数据库：向量嵌入（embedding）
是一等的值类型，相似度检索与图遍历并行工作，引擎直接向智能体（agent）暴露契合其
需求的原语——自然语言查询、实时变更流与时间旅行——而非在传统图数据库之上事后叠加
AI 功能。

与 SQLite 类似，它是**嵌入式**的：以库的形式链接进应用，由单个磁盘文件承载，无需
运维独立的服务进程。但与 SQLite 不同，它同样可以**对外服务**——`drsg serve` 提供
JSON-RPC 2.0 接口、浏览器控制台以及 WebSocket 变更流，并配有五种语言的客户端 SDK。

对于围绕知识图谱、GraphRAG 流水线或智能体长期记忆构建的应用，Dr Strange 力求成为
承载这一切的单一存储。

## 特性

| 能力 | 提供的价值 |
|---|---|
| **平面（Planes）** | 单个数据库中承载多个相互独立的图 |
| **一等向量嵌入** | 向量属性，原生以 HNSW 建立索引 |
| **混合检索** | 融合向量 + 关键词（BM25）+ 图邻近度的检索 |
| **查询语言** | 可序列化的逻辑计划，以及 openCypher 子集 |
| **图算法** | PageRank、连通分量、最短路径、Louvain |
| **自然语言查询** | 以自然语言提问 → 生成计划 → 执行 |
| **时间旅行** | 读取图在过去某次提交或某个时间戳**时刻**的状态 |
| **变更流** | 订阅某个平面并实时接收其变更 |
| **备份 / 恢复** | 一致且保留 id 的全库快照 |
| **接口** | Web 控制台、五种语言 SDK、命令行工具与 MCP 服务器 |

依赖模型的功能（自然语言查询、文档摄取与文本嵌入检索）会调用外部或本地的 LLM；其余
功能均无需任何模型即可运行。参见[附录 B](docs/zh/src/appendix-b.md)。

## 快速上手

```console
# 构建命令行工具（drsg）。
$ cargo build --release -p dr-strange-cli

# 创建一个平面，写入数据并查询。
$ drsg --db graph.drsg plane create social
$ drsg --db graph.drsg cypher --plane social \
    'CREATE (a:Person {name:"Ada"})-[:KNOWS]->(b:Person {name:"Alan"})'

# 启动控制台 + API 服务。
$ drsg --db graph.drsg serve
```

完整的操作流程——构建、磁盘布局、向量嵌入与相似度检索、服务端及其配置，以及容器
镜像——参见手册的**快速上手**章节：
[English](docs/en/src/getting-started.md) · [中文](docs/zh/src/getting-started.md)。

## 文档

手册逐一深入讲解各个部分：
[AI 原生](docs/zh/src/ai-native.md) ·
[查询语言](docs/zh/src/query-language.md) ·
[Web 控制台](docs/zh/src/web-ui.md) ·
[SDK](docs/zh/src/sdk.md) ·
[嵌入式 CLI](docs/zh/src/embedded-cli.md) ·
[MCP](docs/zh/src/mcp.md) ·
[JSON-RPC API 清单](docs/zh/src/appendix-a.md)。

在本地构建（mdBook）：`just docs-serve zh`（中文）或 `just docs-serve`（英文）。

## 架构

Dr Strange 由清晰分层构成——存储（手写的、支持 MVCC 的 LSM 引擎）、带版本戳的
缓存、计算、API 层，以及横贯各层的平面模型——外围的封装层（Web、SDK、CLI、MCP、
LLM）则位于内核之上。

- **[架构章节](docs/zh/src/architecture.md)** —— 分层全景图，以及提交序列
  （commit sequence）如何统一 MVCC、缓存、时间旅行与变更流。
- **[`arch/`](arch/)** —— 各层详细的设计笔记：
  [总览](arch/00-overview.md)、
  [存储](arch/01-storage.md)、
  [缓存](arch/02-cache.md)、
  [计算](arch/03-computation.md)、
  [API](arch/04-api.md)、
  [平面](arch/09-planes.md)。

## 基准测试

针对一款嵌入式图数据库（Kùzu）、通用的嵌入式基线（SQLite）以及业界标准的服务型
数据库（Neo4j）所做的跨引擎对比。每个引擎都加载**相同**的确定性数据集——10 万
节点、50 万条边、128 维向量——并在各自的最优路径上运行**相同**的查询集。

| 操作（中位延迟，↓ 越小越好） | dr-strange | Kùzu | SQLite | Neo4j |
|---|---|---|---|---|
| 按键点查 | **3.4 µs** | 397.6 µs | 5.5 µs | 978.6 µs |
| 一跳扩展 | **6.7 µs** | 2.37 ms | 13.7 µs | 799.5 µs |
| 两跳可达集 | **37.0 µs** | 9.84 ms | 94.7 µs | 1.56 ms |
| 向量 top-k 查询 | **387.7 µs** | 10.39 ms | — | 3.57 ms |

嵌入式 KV 设计带来微秒级的点查与图查询，向量 top-k 延迟低于 Kùzu 与 Neo4j；批量
加载仍落后于成熟的列式引擎。数据均为单次运行、预热后、单机测得——**仅供参考，并非
排行榜**。方法学、注意事项、加载吞吐数据以及复现方式（`just bench-compare`）详见
**[BENCHMARKS.md](BENCHMARKS.md)**。

## 许可证

本项目采用以下任一许可证授权：

- Apache License 2.0（[LICENSE-APACHE](LICENSE-APACHE) 或
  <http://www.apache.org/licenses/LICENSE-2.0>）
- MIT 许可证（[LICENSE-MIT](LICENSE-MIT) 或
  <http://opensource.org/licenses/MIT>）

由你选择其一。

### 贡献

除非你另有明确声明，凡是你有意提交、以纳入本作品的贡献（依 Apache-2.0 许可证的
定义），均应按上述方式进行双重许可，不附带任何额外条款或条件。
