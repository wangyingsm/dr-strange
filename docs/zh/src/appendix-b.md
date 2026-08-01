# 附录 B：是否需要 LLM

Dr Strange 仅为一组特定且有界的功能调用外部语言模型。其余的一切——图存储、向量与
关键词索引、基于预先算得向量的混合检索、图算法、时间旅行、变更流与备份——完全不需要
任何模型即可运行。

本附录明确界定：究竟哪些功能需要模型支持、如何在不使用任何模型的情况下运行
Dr Strange，以及如何将这些模型功能指向一个本地模型而非托管 API。无论哪种情形，提供方
API 密钥都从服务端环境（或 `[llm]` 配置节）读取，绝不从请求或工具参数读取。

## 哪些功能需要 LLM 支持

模型在两种情形下被调用：为文本**生成嵌入**（把一个字符串变成一个向量），以及
**对话**（生成结构或计划）。依赖其一或两者的功能：

| 功能 | 需要 | 原因 |
|---|---|---|
| 为**文本**相似度查询生成嵌入（`SEARCH … NEAR "文本"`、语义 `plane.find`、从文本出发的混合向量信号） | 嵌入提供方 | 查询字符串在检索前于服务端被嵌入 |
| **自然语言查询**（`ask` / `plane.ask`） | 对话提供方（+ 用于有据工具的嵌入提供方） | 模型将问题编译为计划，并可选地调用基于嵌入的 `find_edge` / `find_entity` 工具 |
| **文档导入**（`digest` / AIgest） | 对话 + 嵌入提供方 | 模型抽取实体与关系；实体被嵌入 |

其余一切都无需模型：

- **存储与检索你已有的向量。** 向量是普通属性；声明一个索引，并以**字面量**向量
  检索它（`SEARCH … NEAR $vec`）。只有*文本*查询才需要嵌入。
- **关键词检索。** BM25 纯为词面。
- **图查询与遍历。** `MATCH`、`SEARCH … NEAR $vec`、`plane.query`、
  `plane.neighbors`、`graph.seed` / `graph.expand`。
- **图算法。** PageRank、连通分量、最短路径、Louvain。
- **时间旅行、变更流，以及备份/恢复。**
- **混合检索的关键词信号与图信号**，以及给定字面量向量时的向量信号。

简而言之：仅在把*文本*变成向量、以及驱动 `ask` 与 `digest` 时才需要模型。若你以自己的
流水线为数据生成嵌入，并以字面量向量、关键词与图进行查询，则 Dr Strange 无需任何
模型。

## 在不使用 LLM 的情况下运行 Dr Strange

有两种相互独立的方式在无模型下运行：直接不使用模型功能，以及构建一个将模型代码完全
排除在外的二进制。

### 不使用模型功能

由模型支撑的操作仅在被调用时才访问提供方。不提供任何提供方密钥，并避开 `ask`、
`digest` 与文本嵌入查询，系统的其余部分即完全可用。以你自己的流水线为数据生成嵌入，
将向量存为属性，并以如下方式查询：

- **字面量向量相似度** —— `SEARCH (d:Doc) ON embedding NEAR $vec TOPK 10`；
- **关键词** —— 一个 BM25 索引（`index keyword …`）；
- **图** —— `MATCH`、遍历，以及各图算法。

一个对外提供服务的实例仍然暴露 `ask` 与 `digest`，但在未配置提供方密钥时它们返回一个
清晰的错误；其它一切不受影响。

### 不带模型代码进行构建

命令行工具将模型功能门控在 `digest` Cargo 特性之后，该特性会引入 LLM crate。不带它
构建，即可得到一个完全没有模型依赖的精简二进制：

```console
$ cargo build --release -p dr-strange-cli --no-default-features --features native-backend
```

所得的 `drsg` 省去了 `ask` 与 `digest` 命令；其它每个命令——平面、导入/导出、查询、
索引、混合检索（关键词与字面量向量信号）、算法、快照/恢复，以及 `serve`——均保持
不变。

## 使用本地 LLM / 模型

模型功能并不要求托管 API。一个提供方要么是一个**预设**名，要么是一个**base URL**，
因此任何 OpenAI 兼容的端点——包括本地端点——都可以提供对话与嵌入。

### Ollama

[Ollama](https://ollama.com) 在本地暴露一个 OpenAI 兼容的 API。内置的 `ollama` 预设
指向 `http://localhost:11434/v1`，无需密钥，默认对话用 `llama3.1`、嵌入用
`nomic-embed-text`：

```console
$ ollama pull llama3.1
$ ollama pull nomic-embed-text

$ drsg --db graph.drsg ask "which companies does Ada work for?" \
    --plane social --chat ollama --embed ollama

$ drsg --db graph.drsg digest notes.md --plane social --apply \
    --chat ollama --embed ollama
```

按需以 `--model` 与 `--embed-model` 覆盖模型。

### 任意 OpenAI 兼容服务

一个讲 OpenAI API 的本地推理服务——vLLM、LM Studio、llama.cpp 的服务，及其它——通过
将其 **base URL** 作为提供方传入来指定，并显式命名模型：

```console
$ drsg --db graph.drsg ask "…" --plane social \
    --chat  http://localhost:8000/v1 --model       my-chat-model \
    --embed http://localhost:8000/v1 --embed-model my-embed-model
```

若该端点需要密钥，请在环境中设置它，并以密钥环境变量选项命名其变量名；一个无需密钥的
本地服务则什么都不需要。

### 在仪表盘、服务与 MCP 中

在所有使用模型之处，同一套提供方均适用。**AIgest** 页与语义搜索选择器会列出这些
预设，包括 `ollama`；服务与 MCP 宿主从各自的环境读取提供方密钥（对于无需密钥的本地
服务则不读取）。对话提供方与嵌入提供方是各自独立选择的，因此一个本地对话模型可以与
一个本地——或托管——嵌入模型搭配，反之亦然。
