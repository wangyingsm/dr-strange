# MCP

`drsg-mcp` 是一个 [Model Context Protocol](https://modelcontextprotocol.io)
服务，它内嵌 Dr Strange，并将数据库作为一组工具暴露给 LLM 智能体。兼容的宿主中的
智能体可以直接检索、遍历、查询、运行算法、以自然语言提问、导入文档——以及写入图——
而无需专门的集成代码。

## Dr Strange 如何契合智能体

MCP 本身就是 JSON-RPC 2.0——与 Web 后端所讲的是同一套协议——因此一个 MCP 服务是一等
的接口，而非一层适配器。`drsg-mcp` 内嵌 `dr-strange-core`，**在进程内**打开数据库
（一如命令行），随后通过 **stdio** 提供该协议：宿主启动该进程，并在其标准输入与输出
上交换 JSON-RPC 消息。日志写入 stderr 与一个滚动文件，绝不写入承载协议的 stdout。

## 运行与配置

`drsg-mcp` 是一个独立的二进制。一行命令即可安装发行版本
（[第 2 章](./getting-started.md#安装发行版二进制)）：

```console
$ curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh -s -- --bin drsg-mcp
```

在 Windows 的 PowerShell 中：

```console
PS> & ([scriptblock]::Create((irm https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.ps1))) -Bin drsg-mcp
```

它同样可以从源码构建：`cargo build --release -p dr-strange-mcp`。它以其第一个参数
作为数据库路径，其次为 `$DRSG_DB`，再次为 `graph.drsg`：

```console
$ drsg-mcp /path/to/graph.drsg
```

它通常由宿主启动，而非手动运行。宿主通过命令、参数与环境对其进行配置——环境携带图
工具所需的任何 LLM 提供方密钥：

```json
{
  "mcpServers": {
    "dr-strange": {
      "command": "drsg-mcp",
      "args": ["/path/to/graph.drsg"],
      "env": {
        "OPENAI_API_KEY": "sk-...",
        "DEEPSEEK_API_KEY": "...",
        "DASHSCOPE_API_KEY": "..."
      }
    }
  }
}
```

`crates/dr-strange-mcp/mcp.json` 提供了一份可直接编辑的该配置副本；将 `command` 设为
`drsg-mcp`（位于 `PATH` 中）或该二进制的绝对路径，令 `args` 指向你的数据库，并只提供
你所使用的提供方密钥。

由于它在进程内打开数据库，`drsg-mcp` 不应对一个 `drsg serve` 当前正持有打开的数据库
运行。

## 工具

| 工具 | 类别 | 用途 |
|---|---|---|
| `list_planes` | 读 | 列出各平面及其节点/边计数 |
| `describe_plane` | 读 | 某个平面的软 schema（标签、属性、边类型） |
| `get_node` | 读 | 按 id 或外部键获取一个节点 |
| `search` | 读 | 向量相似度——最近的 *k* 个节点 |
| `traverse` | 读 | 从某个节点进行邻域扩展（1 跳及以上） |
| `query` | 读 | 运行一个序列化的逻辑计划 |
| `cypher` | 读 | 运行一条 openCypher 子集语句 |
| `algo` | 读 | 一个图算法（pagerank / components / shortest_path / louvain） |
| `hybrid` | 读 | 融合的向量 + 关键词 + 图邻近度检索 |
| `ask` | 读 | 一个自然语言问题，编译为计划并执行 |
| `write_nodes` | 写 | 创建节点（批量） |
| `write_edges` | 写 | 按端点键创建边（批量） |
| `create_plane` | 写 | 创建一个空平面 |
| `drop_plane` | 写 | 删除一个平面及其内容（需确认） |
| `digest` | 写 | 导入一篇文档（默认为 dry-run） |

## 与系统其余部分的映射

这些工具即经由命令行与 JSON-RPC 接口所能进行的相同操作，只是适配了智能体的需要：
`search` / `traverse` / `query` / `cypher` / `algo` / `hybrid` / `ask` 对应
[第 4 章](./query-language.md)与[第 3 章](./ai-native.md)的查询与检索接口；
`write_nodes` / `write_edges` / `create_plane` / `drop_plane` / `digest` 对应写入
与导入接口。每个工具都以该平面的软 schema 为依据，而 `describe_plane` 暴露该 schema，
使智能体得以在行动之前先了解一张图。

## 安全

- **提供方密钥从服务端环境读取，绝不从工具参数读取**——智能体无法通过一次调用外泄
  或提供密钥。
- **读操作是非破坏性的。** 尤其是 `ask`，它编译为一个只读计划，无法改动图。
- **破坏性写入受到保护。** `drop_plane` 需要一个显式的确认标志，而 `digest` 默认为
  dry-run，返回拟议的节点与边以供检视，而非直接写入。

## 示例：一个智能体工作流

一次典型的智能体会话会组合这些工具：

1. `describe_plane`，以了解作用范围内的标签、属性与边类型。
2. `search` 或 `hybrid` 检索相关节点，再以 `traverse` 收集其邻域——为模型提供扎实的
   上下文。
3. `ask` 或 `cypher`，针对图回答一个具体问题。
4. `digest`（先 dry-run，再应用），将新的原始材料作为实体与关系并入图。

由于存储的是一张图，智能体得以在同一会话中从检索转入遍历——即[第 1 章](./what-is-dr-strange.md)
所述、由模型端到端驱动的 GraphRAG 循环。
