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

由于它在进程内打开数据库，**同一时刻只能有一个进程**直接打开某个数据库——可以是一个
`drsg-mcp`、一条 `drsg` 命令或一个 `drsg serve`，但不能同时有两个。这是强制的，而非
建议：第二次打开会以明确的错误失败，而不是把数据库弄坏。

这一点对智能体宿主尤为要紧，因为每个宿主都会各自派生自己的 MCP 服务子进程。同一个项目
开着两个编辑器，就意味着两个 `drsg-mcp` 进程，而后者将拒绝启动。若干智能体需要共享同一
份记忆的场景，见下一节。

## 跨智能体共享：`drsg serve` 上的 `/mcp`

`drsg-mcp` 按设计内嵌运行：把宿主指向一个路径即可工作，无需运行任何附加服务，也无需
配置。这对单个智能体是正确答案，但也正因如此，两个宿主无法直接共享一个数据库——因为
各自都在打开这个文件。

`drsg serve`（arch/08）已经为 JSON-RPC 接口和仪表盘解决了"一个写者、多个客户端"的问题。
它在 `POST /mcp` 上暴露完全相同的工具集，走 MCP 的 Streamable HTTP 传输，让多个智能体
宿主可以指向同一个服务，而不必各自内嵌一份数据库副本：

```json
{
  "mcpServers": {
    "dr-strange": {
      "url": "http://127.0.0.1:7700/mcp",
      "headers": { "Authorization": "Bearer <DRSG_TOKEN>" }
    }
  }
}
```

（请查阅你所用宿主的文档以确认其远程 MCP 服务配置的确切格式——以上 `url`/`headers`
字段仅为示例，并非通用格式。）

`/mcp` 的鉴权方式与 `/rpc` 完全一致：未设置 `DRSG_TOKEN` 时，只有同源的浏览器 UI
受信任，**任何程序化客户端都会被拒绝，读操作也不例外**——零配置的桌面安装不应在
本地悄悄暴露一个开放的 API。请在服务端设置 `DRSG_TOKEN`，并将其作为 bearer token
传递，以便从任何其他地方（包括另一个智能体宿主）访问 `/mcp`。

把它放到某个域名后面之前，有一处限制值得先知道：MCP 传输层会校验请求的 `Host` 头，
只放行回环地址（`localhost`、`127.0.0.1`、`::1`），以此削弱针对本机服务的 DNS
重绑定攻击，其余一律返回 **403**。这道检查在这里是有价值的——未设 token 的服务端
信任自己的同源 UI，而这正是重绑定要冒充的东西——但它也意味着 `/mcp` 只在
`http://127.0.0.1:7700/mcp` 上应答，而不在 `https://memory.example.com/mcp` 上，
尽管同一个服务端的 `/rpc` 可以。ROADMAP §10 所针对的场景——同一台机器上的多个
智能体宿主——不受影响。

每个会话都会得到自己的 `DrStrange` 实例，但它们共享服务端打开的同一个 `Database`——
一个会话的写入会立即对其他所有会话可见，正如两个浏览器标签页对 `/rpc` 的访问那样。
这正是关键所在：核心内部的 `write_gate` 会序列化并发写者，因此这是安全的，而不仅仅是
方便。

`digest` 的 LLM 调用仍然花费服务进程自身的提供方密钥（无论本地还是远程，都绝不来自
工具参数）；`write_nodes`/`write_edges` 保留其逐次调用的批量原子性，因为无论哪种情况，
工具代码都在同一进程内针对同一个 `Database` 运行——这里不会代理到 `/rpc`，也不会改变
任何工具的行为。`[digest]` 配置段对 `digest` 工具的作用与它对 `/rpc` 上 `digest.run`
的作用完全一致：为规避提供方限流而调低 `concurrency`，两个入口都会遵守，不存在一个听
另一个不听的情况。（内嵌的 `drsg-mcp` 二进制没有配置文件，沿用内置默认值。）

### 会话生命周期

正常退出的 host 会发送 `DELETE /mcp`，其会话立即消失。被 `SIGKILL` 的 host——比如编辑
器重启它的 MCP 子进程——什么都不会发，服务端改用定时器回收：**空闲 10 分钟**，或会话创
建后 **60 秒**内没有收到 `initialize`。该会话的 worker task、它的 `DrStrange`、以及缓
冲的消息都随之释放。

空闲窗口取 10 分钟而非 5 分钟，原因很具体：传输层会把*正在执行*的工具计为空闲——其
keep-alive 计时器只被流量重置，而一次工具调用在派发与返回结果之间不发送任何内容。因此
在一个本就安静的会话上，运行时间超过该窗口的工具会被中途拆除。10 分钟足以覆盖任何现实
中的 `digest`；若你经常运行更久的任务，请保持会话有流量，或预期需要重试。

回收后残留的是每个死会话一条 map 条目。它只有几十字节，但并非无害：该会话 id 上的下一
个请求会得到 **500**，而不是规范要求的 **404**，于是本应据此重新 `initialize` 的客户端
不会重连，该会话将一直损坏到 host 重启为止。这两点都属于传输层问题，应在上游修复而非
在此绕开。若你在脚本里循环创建会话，记得关闭，即可完全避开这一区域。

### 工具并发

工具调用与 HTTP 请求分别限流，全进程同时最多 **16 个**（若 `max_concurrent` 更小则取
更小者）。二者并非一回事：`max_concurrent` 统计的是请求，其中大多数开销很低；而每次工具
调用都是一次全图扫描、批量写入，或一次向 LLM 扇出的 digest。此外，传输层在工具调用刚被
*入队*时就予以应答，因此请求上限在工作真正开始之前就已释放，无法对其构成约束。超出的调用
会排队而非失败——服务器繁忙时应让 agent 等待，而不是拒绝它。

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
| `digest` | 写 | 导入一篇文档（默认为 dry-run；`mode` 决定抽取精度） |

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
