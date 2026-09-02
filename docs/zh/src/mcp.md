# MCP

`drsg-mcp` 是一个 [Model Context Protocol](https://modelcontextprotocol.io)
服务，它内嵌 Dr Strange，并将数据库作为一组工具暴露给 LLM 智能体。兼容的宿主中，
智能体可以直接检索、遍历、查询、运行算法、以自然语言提问、导入文档，也可以写入
图，全程无需专门的集成代码。

## Dr Strange 如何契合智能体

MCP 本身就是 JSON-RPC 2.0，与 Web 后端说的是同一套协议，因此 MCP 服务是一等的
接口，而不是一层适配器。`drsg-mcp` 内嵌 `dr-strange-core`，在进程内打开数据库
（和命令行一样），再通过 **stdio** 提供该协议：宿主启动这个进程，双方在标准输入
输出上交换 JSON-RPC 消息。日志写入 stderr 与一个滚动文件，绝不写入承载协议的
stdout。

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

它同样可以从源码构建：`cargo build --release -p dr-strange-mcp`。数据库以
`--db <路径>` 指定，也可以直接作为一个裸参数给出；其次读 `$DRSG_DB`，再退回到默认
值 `graph.drsg`。`--help` 与 `--version` 只打印信息随即退出：

```console
$ drsg-mcp --db /path/to/graph.drsg
$ drsg-mcp /path/to/graph.drsg          # 同样的意思，简写
```

数据库必须已经存在——这个服务从不创建数据库。用 `drsg digest <目录> --apply --db
<路径>` 建立，或在仓库中运行 `drsg init`。空数据库对任何问题都只会答「什么也没
找到」，这和一次出了错的图化毫无区别；因此路径不存在时直接报错。

**首选 `drsg init`。** 它会图化本仓库、在后台拉起一个 `drsg serve … watch`，并把
该服务的 URL 写进项目的 `.mcp.json`——此后平面会跟随仓库的提交更新，而所有宿主共用
这一个实例（见[下一节](#跨智能体共享drsg-serve-上的-mcp)）。`drsg-mcp` 是没有这样一个
服务时的后备：只会说 stdio 的宿主，或者无人监视的数据库。把两者同时指向同一个数据库
是行不通的，原因见下一段。

它通常由宿主启动，而不是手动运行。宿主通过命令、参数与环境变量来配置它，图工具
所需的 LLM 提供方密钥就放在环境变量里传入：

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

`crates/dr-strange-mcp/mcp.json` 提供了一份可直接编辑的该配置副本：把 `command`
设为 `drsg-mcp`（位于 `PATH` 中）或该二进制的绝对路径，让 `args` 指向你的数据库，
只提供你实际用到的提供方密钥。

由于它在进程内打开数据库，同一时刻只能有一个进程能直接打开某个数据库：可以是
一个 `drsg-mcp`、一条 `drsg` 命令，或一个 `drsg serve`，但不能两个同时打开。这
一点是强制的，不只是建议——第二次打开会直接报错失败，而不会把数据库弄坏。

这一点对智能体宿主尤为要紧，因为每个宿主都会各自派生自己的 MCP 服务子进程。同一
个项目开着两个编辑器，就意味着两个 `drsg-mcp` 进程，后者会拒绝启动。若干智能体
需要共享同一份记忆的场景，见下一节。

## 跨智能体共享：`drsg serve` 上的 `/mcp`

`drsg-mcp` 按设计内嵌运行：把宿主指向一个路径即可工作，无需运行任何附加服务，也
无需配置。这对单个智能体是正确答案，但也正因为如此，两个宿主没法直接共享一个
数据库：两边都要打开同一个文件。

`drsg serve`（arch/08）已经为 JSON-RPC 接口和仪表盘解决了"一个写者、多个客户端"
的问题。它在 `POST /mcp` 上暴露完全相同的工具集，走 MCP 的 Streamable HTTP 传输，
让多个智能体宿主可以指向同一个服务，不必各自内嵌一份数据库副本：

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

（远程 MCP 配置的确切格式请查阅你所用宿主的文档。上面的 `url`/`headers` 字段只是
示例，不是通用格式。）

`/mcp` 的鉴权方式与 `/rpc` 完全一致：未设置 `DRSG_TOKEN` 时，只有同源的浏览器 UI
受信任，任何程序化客户端都会被拒绝，读操作也不例外，因为零配置的桌面安装不该在
本地悄悄暴露一个开放的 API。要从其他地方（包括另一个智能体宿主）访问 `/mcp`，请
在服务端设置 `DRSG_TOKEN`，并以 bearer token 的形式传递它。

把它放到某个域名后面之前，有一点需要先知道：MCP 传输层会校验请求的 `Host` 头，
只放行回环地址（`localhost`、`127.0.0.1`、`::1`），其余一律返回 **403**，借此削弱
针对本机服务的 DNS 重绑定攻击。这道检查确实有价值——未设 token 的服务端信任自己
的同源 UI，而重绑定攻击正是要冒充这个身份——但它也意味着 `/mcp` 只在
`http://127.0.0.1:7700/mcp` 上应答，换成 `https://memory.example.com/mcp` 就不行
了，尽管同一个服务端的 `/rpc` 可以。ROADMAP §10 针对的场景——同一台机器上的多个
智能体宿主——不受影响。

每个会话都会拿到自己的 `DrStrange` 实例，但它们共享服务端打开的同一个
`Database`：一个会话的写入会立刻对其他所有会话可见，就像两个浏览器标签页访问
`/rpc` 那样。这正是关键所在——核心内部的 `write_gate` 会把并发写者串行化，所以
这么做是安全的，不只是方便。

有一项能力的差异来自传输方式本身，而不是配置。`digest` 接受一个 `path`，由服务端
读取该文档（Word、PowerPoint、Excel、OpenDocument、RTF、EPUB、CSV、PDF、Markdown
或纯文本），但只有 stdio 服务接受这种用法：这个进程运行在你自己的机器上、以你
自己的身份运行，它读到的文件本来就是 agent 能打开的文件。共享的 `drsg serve` 就
会拒绝这么做——一旦允许调用方指定任意路径，一个已通过认证的远程 agent 就能把
服务器上的任意文件读进图里，再用查询取回。在 `/mcp` 上，请改为直接以 `text` 传入
文档内容。

`digest` 的 LLM 调用，花的始终是服务进程自身的提供方密钥（不管本地还是远程，都
绝不会来自工具参数）。`write_nodes`/`write_edges` 仍然保留逐次调用的批量原子性，
因为不管走哪条路径，工具代码都在同一进程内针对同一个 `Database` 运行，不会代理到
`/rpc`，也不会改变任何工具的行为。`[digest]` 配置段对 `digest` 工具的作用，和它对
`/rpc` 上 `digest.run` 的作用完全一致：调低 `concurrency` 来规避提供方限流时，
两个入口都会遵守，不存在一个听、另一个不听的情况。（内嵌的 `drsg-mcp` 二进制没有
配置文件，沿用内置默认值。）

### 会话生命周期

正常退出的 host 会发送 `DELETE /mcp`，会话随即消失。被 `SIGKILL` 的 host（比如
编辑器重启它的 MCP 子进程）什么都不会发，服务端就改用定时器回收：空闲超过 10
分钟，或者会话创建后 60 秒内没有收到 `initialize`。这时该会话的 worker task、它的
`DrStrange`，以及缓冲的消息都会一并释放。

空闲窗口取 10 分钟而不是 5 分钟，原因很具体：传输层会把正在执行的工具调用也算作
空闲，因为它的 keep-alive 计时器只认流量，而一次工具调用从派发到返回结果之间不会
发送任何东西。所以在一个本就安静的会话上，运行时间超过这个窗口的工具会被中途
拆断。10 分钟足以覆盖任何现实场景中的 `digest`；如果你经常要跑更久的任务，要么让
会话保持有流量，要么就要预期它可能需要重试。

回收之后，每个死会话都会留下一条 map 条目。它只有几十字节，但并非无害：这个
会话 id 上的下一个请求会收到 **500**，而不是规范要求的 **404**，于是本该据此重新
`initialize` 的客户端不会重连，这个会话就一直坏着，直到 host 重启为止。这两点都是
传输层的问题，该在上游修，而不是在这里绕过去。如果你在脚本里循环创建会话，记得
手动关闭，就能完全避开这个问题。

### 工具并发

工具调用和 HTTP 请求是分开限流的：全进程同时最多 **16 个**工具调用（如果
`max_concurrent` 更小，则取更小的那个）。二者不是一回事——`max_concurrent` 统计的
是请求，其中大多数开销很低；而每次工具调用，要么是一次全图扫描、一次批量写入，
要么是一次向 LLM 扇出的 digest。而且传输层在工具调用刚被*入队*时就会应答，请求
上限在工作真正开始之前就已经释放了，根本管不到它。超出的调用会排队而不是直接
失败：服务器繁忙时该让 agent 等，而不是把它拒之门外。

## 工具

| 工具 | 类别 | 用途 |
|---|---|---|
| `list_planes` | 读 | 列出各平面及其节点/边计数 |
| `describe_plane` | 读 | 某个平面的软 schema（标签、属性、边类型） |
| `get_node` | 读 | 按 id 或外部键获取一个节点 |
| `search` | 读 | 语义查找——嵌入查询文本，返回最近的 *k* 个节点 |
| `context` | 读 | 已图化代码平面上一个符号的完整邻域——首选的智能体动词 |
| `describe` | 读 | 一个符号的属性，只看节点的轻量视图 |
| `grep` | 读 | 在被监视的源码树上做字面文本检索 |
| `trace` | 读 | 两个符号之间图中记录的最短调用路径 |
| `impact` | 读 | 所有能到达该符号的东西，按距离分组 |
| `snippet` | 读 | 一个符号的源码文本 |
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

这些工具做的事，和命令行、JSON-RPC 接口能做的完全一样，只是照顾了智能体的使用
方式：`search` / `traverse` / `query` / `cypher` / `algo` / `hybrid` / `ask` 对应
[第 4 章](./query-language.md)与[第 3 章](./ai-native.md)的查询与检索接口；
`write_nodes` / `write_edges` / `create_plane` / `drop_plane` / `digest` 对应写入
与导入接口。每个工具都依据该平面的软 schema 行事，而 `describe_plane` 就是用来
暴露这份 schema 的，让智能体能在动手之前先了解这张图。

## 安全

- **提供方密钥从服务端环境读取，绝不从工具参数读取**——智能体无法通过一次调用
  外泄或提供密钥。
- **读操作是非破坏性的。** 尤其是 `ask`，它编译为一个只读计划，无法改动图。
- **破坏性写入受到保护。** `drop_plane` 需要一个显式的确认标志，而 `digest` 默认
  为 dry-run，返回拟议的节点与边以供检视，而非直接写入。

## 示例：一个智能体工作流

一次典型的智能体会话会组合这些工具：

1. `describe_plane`，以了解作用范围内的标签、属性与边类型。
2. `search` 或 `hybrid` 检索相关节点，再以 `traverse` 收集其邻域，为模型提供扎实
   的上下文。
3. `ask` 或 `cypher`，针对图回答一个具体问题。
4. `digest`（先 dry-run，再应用），将新的原始材料作为实体与关系并入图。

由于存储的是一张图，智能体可以在同一个会话里从检索直接转入遍历——这正是
[第 1 章](./what-is-dr-strange.md)所说的、由模型端到端驱动的 GraphRAG 循环。
