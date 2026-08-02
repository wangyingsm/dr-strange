<p align="center">
  <img src="crates/dr-strange-web/frontend/public/magic-circle.svg" alt="Dr Strange" width="120" height="120">
</p>

<h1 align="center">Dr Strange</h1>

<p align="center"><em>一个 AI 原生的嵌入式图数据库，使用 Rust 编写。</em></p>

<p align="center">
  <a href="https://github.com/wangyingsm/dr-strange/actions/workflows/ci.yml"><img src="https://github.com/wangyingsm/dr-strange/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/wangyingsm/dr-strange/actions/workflows/release.yml"><img src="https://github.com/wangyingsm/dr-strange/actions/workflows/release.yml/badge.svg" alt="Release"></a>
  <a href="https://github.com/wangyingsm/dr-strange/actions/workflows/docs.yml"><img src="https://github.com/wangyingsm/dr-strange/actions/workflows/docs.yml/badge.svg" alt="Docs"></a>
  <a href="https://github.com/wangyingsm/dr-strange/releases/latest"><img src="https://img.shields.io/github/v/release/wangyingsm/dr-strange?label=release&color=blue" alt="Latest release"></a>
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue" alt="License: MIT OR Apache-2.0"></a>
</p>

<p align="center"><a href="README.md">English</a> · <strong>简体中文</strong></p>

📖 **Dr Strange 手册** —— 完整教程与指南：
[English](https://wangyingsm.github.io/dr-strange/en/book/introduction.html) ·
[中文](https://wangyingsm.github.io/dr-strange/zh/book/introduction.html)。

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

## Web UI 截图

<table>
  <tr>
    <td width="50%"><a href="screenshots/00.jpg"><img src="screenshots/00.jpg" width="100%" alt="Dashboard —— 平面统计与管理"></a><br><sub><b>Dashboard</b> —— 实时的平面统计与管理</sub></td>
    <td width="50%"><a href="screenshots/01.jpg"><img src="screenshots/01.jpg" width="100%" alt="Explore —— 交互式图谱与节点详情"></a><br><sub><b>Explore</b> —— 交互式图谱与节点详情</sub></td>
  </tr>
  <tr>
    <td width="50%"><a href="screenshots/02.jpg"><img src="screenshots/02.jpg" width="100%" alt="Algorithms —— 图上的最短路径"></a><br><sub><b>Algorithms</b> —— PageRank、社区发现与最短路径</sub></td>
    <td width="50%"><a href="screenshots/03.jpg"><img src="screenshots/03.jpg" width="100%" alt="AIgest —— LLM 文档摄取，抽取实体与关系"></a><br><sub><b>AIgest</b> —— LLM 文档摄取，抽取实体与关系</sub></td>
  </tr>
</table>

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
功能均无需任何模型即可运行。参见[附录 C](https://wangyingsm.github.io/dr-strange/zh/book/appendix-c.html)。

## 安装

一行命令，无需任何工具链。安装脚本会下载对应平台的发行版二进制、校验其 SHA-256，
并将其放入 `PATH`。可安装的二进制有两个：命令行与服务端 `drsg`，以及面向 LLM
智能体的 MCP 服务 `drsg-mcp`。

**Linux**

```console
# 命令行与服务端 —— drsg
$ curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh

# MCP 服务 —— drsg-mcp
$ curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh -s -- --bin drsg-mcp
```

**macOS**（同一个脚本；Apple 芯片与 Intel 均可）

```console
# 命令行与服务端 —— drsg
$ curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh

# MCP 服务 —— drsg-mcp
$ curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh -s -- --bin drsg-mcp
```

**Windows**（PowerShell）

```console
# 命令行与服务端 —— drsg
PS> irm https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.ps1 | iex

# MCP 服务 —— drsg-mcp（以代码块方式运行：管道送入的脚本无法接收参数）
PS> & ([scriptblock]::Create((irm https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.ps1))) -Bin drsg-mcp
```

`--bin all` 同时安装两个二进制；`--version v1.1.0` 指定某个发行版本，`--dir <path>`
指定安装目录（默认 `~/.local/bin`，Windows 上为 `%LOCALAPPDATA%\Programs\drsg\bin`）。
在 Windows 上对应的参数为 `-Bin`、`-Version` 与 `-Dir`。

其他方式：容器镜像 `ghcr.io/wangyingsm/dr-strange:latest`，或
[发行页](https://github.com/wangyingsm/dr-strange/releases)上的归档包与校验和。

**从源码构建**——最后的选择，适用于没有发布二进制的平台，或需要构建工作副本的场景。
需要 [Rust 工具链](https://rustup.rs)；仪表盘在编译期嵌入二进制，因此请先构建单页应用
（`just web-build`，需要 [bun](https://bun.sh)），否则二进制中只会带有占位页。

```console
$ cargo build --release -p dr-strange-cli   # → target/release/drsg
$ cargo build --release -p dr-strange-mcp   # → target/release/drsg-mcp
```

## 快速上手

```console
# 创建一个平面，写入数据并查询。
$ drsg --db graph.drsg plane create social
$ drsg --db graph.drsg cypher --plane social \
    'CREATE (a:Person {name:"Ada"})-[:KNOWS]->(b:Person {name:"Alan"})'

# 启动控制台 + API 服务。
$ drsg --db graph.drsg serve
```

完整的操作流程——构建、磁盘布局、向量嵌入与相似度检索、服务端及其配置，以及容器
镜像——参见手册的**快速上手**章节：
[English](https://wangyingsm.github.io/dr-strange/en/book/getting-started.html) ·
[中文](https://wangyingsm.github.io/dr-strange/zh/book/getting-started.html)。

## 文档

手册逐一深入讲解各个部分：
[AI 原生](https://wangyingsm.github.io/dr-strange/zh/book/ai-native.html) ·
[查询语言](https://wangyingsm.github.io/dr-strange/zh/book/query-language.html) ·
[Web 控制台](https://wangyingsm.github.io/dr-strange/zh/book/web-ui.html) ·
[SDK](https://wangyingsm.github.io/dr-strange/zh/book/sdk.html) ·
[嵌入式 CLI](https://wangyingsm.github.io/dr-strange/zh/book/embedded-cli.html) ·
[MCP](https://wangyingsm.github.io/dr-strange/zh/book/mcp.html) ·
[JSON-RPC API 清单](https://wangyingsm.github.io/dr-strange/zh/book/appendix-a.html) ·
[查询语言文法](https://wangyingsm.github.io/dr-strange/zh/book/appendix-b.html)。

在本地构建（mdBook）：`just docs-serve zh`（中文）或 `just docs-serve`（英文）。

## 架构

Dr Strange 由清晰分层构成——存储（手写的、支持 MVCC 的 LSM 引擎）、带版本戳的
缓存、计算、API 层，以及横贯各层的平面模型——外围的封装层（Web、SDK、CLI、MCP、
LLM）则位于内核之上。

- **[架构章节](https://wangyingsm.github.io/dr-strange/zh/book/architecture.html)** —— 分层全景图，以及提交序列
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
