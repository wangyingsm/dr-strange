# drsg — 代码智能图谱 MCP 服务

把代码仓库消化成一张**已解析的调用图**，让编码智能体一次往返就能回答
「谁调用了它」「改它会波及什么」「X 如何到达 Y」「哪些 HTTP 路由相接」。
全程不经过模型，完全在本地运行，代码不出网。

它同时是一个嵌入式图数据库：单文件、零运维，向量嵌入是一等值类型，
支持 openCypher 子集、混合检索、图算法、时间旅行与变更流。

---

## 为什么需要它

问智能体一句*改这里会影响什么*。没有图，它只能 grep 一个名字，再把匹配到的
文件逐个读完。在 drsg 自己的仓库上实测：

| | |
|---|---|
| 消化本仓库——9.7 万行、13 个解析插件 | **3.1 秒**，全程无模型参与 |
| 回答「谁调用了它、改动会波及到哪里」 | 中位 **925** token，对照 **53,720**——约 **1/61**（n=12） |

两个数字都是实测而非估算，且都可复现。方法、脚本与排除项记在仓库的
`BENCHMARKS.md`：若把 `new` 这类通用名称也算进去，比值会变成 2,600 倍，
所以通用名称的符号被排除在样本之外——这一点是写明的，不是藏起来的。

---

## 安装

**注意：两个二进制。** `drsg` 是 CLI 与服务器，`drsg-mcp` 是 stdio MCP 服务器。
用哪种接入方式，决定你需要装哪个（见下文「接入方式」）。

**一行安装脚本**（Linux / macOS，含 Apple Silicon）

```bash
# 只装 drsg
curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh
# 两个都装（stdio 接入需要）
curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh -s -- --bin all
```

Windows（PowerShell）：

```powershell
irm https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.ps1 | iex
```

脚本会下载对应平台的预编译二进制、校验 SHA-256，并放到 `PATH` 上。

**从包管理器**

```bash
npm install -g @yingge/drsg     # 或 npx @yingge/drsg，不必安装；只含 drsg
cargo install drsg              # 只含 drsg
cargo install dr-strange-mcp    # 只含 drsg-mcp
```

npm 分发的就是安装脚本所下载的那个预编译二进制，控制台已嵌入其中。
`cargo install` 从源码编译，该过程没有 bun，因此 Web 控制台会是一张占位页面；
CLI、MCP 服务器与 JSON-RPC 后端无论哪种方式都是完整的。

**国内网络**：GitHub 发行版从某些网络访问虽然通，却慢到看起来像卡死。
安装脚本支持 `--mirror <base-url>`（环境变量 `DRSG_MIRROR`）指定同布局的镜像：
仍然先试 GitHub，失败或龟速时由镜像接手。无论哪一方应答，SHA-256 都会被校验——
镜像只被信任其可用性，而从不被信任其内容。

---

## 快速上手

```bash
drsg plugin install    # 从官方目录中挑选解析器；输入 0 表示全选
drsg init              # 消化当前仓库、起服务，并写入智能体会读的 MCP 配置
```

`drsg init` 做三件事：消化当前工作区，起一个把每次提交折叠进图的 watcher，
写入 `.mcp.json`（以及为本仓库或本机留有痕迹的那些智能体写好对应配置）。
此后图会随提交自动更新，不需要手动重新消化。

不用 `init` 时，手动建库：

```bash
drsg digest ./my-repo --apply --db graph.drsg
```

**MCP 服务器不会创建数据库。** 它只打开已经存在的库——先 `init` 或 `digest`。

---

## 接入方式

本服务提供 **Stdio** 与 **Streamable HTTP** 两种，不提供 SSE。

### Stdio（需要 `drsg-mcp`）

```json
{
  "mcpServers": {
    "drsg": {
      "command": "drsg-mcp",
      "args": [],
      "env": {}
    }
  }
}
```

不带参数是刻意的：`drsg-mcp` 会向上寻找最近的 `.mcp.json`（像 git 找自己的
目录那样，不越过仓库根；他人所有或任何人可写的文件会被跳过）。若其中声明了
drsg 服务且该服务应答，本进程就把会话**转发**过去——于是一个由 `drsg init`
准备过的仓库，是通过那个已经持有其数据库、且平面与提交保持同步的
`drsg serve … watch` 来回答的，而不会把库打开第二次。

否则就地打开数据库，顺序为：`--db <path>` → 环境变量 `DRSG_DB` → `./graph.drsg`。
需要固定库路径时，在「环境变量配置」里加 `DRSG_DB`。

### Streamable HTTP（只需要 `drsg`）

```json
{
  "mcpServers": {
    "drsg": {
      "type": "http",
      "url": "http://127.0.0.1:7700/mcp"
    }
  }
}
```

先起服务：

```bash
drsg serve --db graph.drsg          # 默认监听 127.0.0.1:7700
drsg serve watch --db graph.drsg    # 并跟随 git 提交增量更新（--dir 默认为 .）
```

`127.0.0.1:7700` 是默认地址，可用 `--addr` 或 `config.toml` 的 `[server].addr`
覆盖。若启动时设置了 `DRSG_TOKEN`，客户端还需带上
`"headers": {"Authorization": "Bearer <token>"}`；未设置令牌时服务只应答
回环地址，这正是本地使用的常态。

---

## 工具清单

### 代码：读与理解

| 工具 | 用途 |
|---|---|
| `context` | **主力工具**。一个符号的全部：定义、签名、文档、调用方（含调用点）、被调用方，以及其余每一条边——一次调用 |
| `snippet` | 精确源码文本。用它代替 `cat` / `sed -n` / 直接读文件 |
| `grep` | 在图背后的源码树上做文本检索；每条命中都会指出它所在的符号 |
| `search` | 不知道标识符时的语义检索（需平面已向量化） |
| `describe` | 只要一个符号的属性，不要边 |
| `trace` | 一个符号如何到达另一个：最短的 CALLS 路径，一跳一行 |
| `impact` | 影响面：改动这个符号会波及到什么，按距离分组并给出精确计数 |
| `fathom` | 近距离细读图的某一片区域 |
| `wire` | 什么在服务某条 HTTP 路径，又是什么在请求它——跨层路由对接 |
| `recall` | 任意 git 修订版本下的代码。代替 `git show <rev>:<path>`、`git grep <rev>`、`git log -p` |
| `history` | 仓库历史概览：HEAD 在哪、分支、标签、合并 |
| `deployed` | 一个正在运行的东西是从哪次提交构建的 |

### 图：通用查询

| 工具 | 用途 |
|---|---|
| `cypher` | openCypher 子集语句 |
| `query` | 运行序列化的逻辑查询计划 |
| `traverse` | 从某个节点做邻域扩展（默认 1 跳） |
| `get_node` | 按 `id` 或外部 `key` 取单个节点 |
| `hybrid` | 混合检索：向量相似度与其他信号融合 |
| `ask` | 自然语言提问 |
| `algo` | 在平面（或某个 `label`）上运行图算法 |

### 平面与数据管理

| 工具 | 用途 |
|---|---|
| `list_planes` / `describe_plane` | 列出平面及其节点/边计数；某个平面的软模式目录 |
| `create_plane` / `drop_plane` | 新建空平面；删除平面及其全部内容 |
| `rebuild` | 重读整棵树重建平面 |
| `attach` / `detach` / `attachments` | 登记本平面可以询问的其他平面 |
| `mount` / `unmount` / `mounts` | 把另一个数据库的平面一并服务；查看各部分来自哪里 |
| `write_nodes` / `write_edges` | 批量建节点 / 按端点外部键批量建边 |
| `digest` | 通过 LLM 把一篇文档消化进图：抽取带类型的实体与关系 |

---

## 关于答案的边界

这是 drsg 与「扁平符号索引」的区别，也是值得单独说明的一点：

- **同名符号返回候选，而不是猜一个。** 名称有歧义时，返回的是候选清单，
  由调用方用 `::` / `.` 后缀或更完整的限定名去收窄。
- **每个动词都报告自己实际走过了什么**——跳数、边的种类、深度上限——
  而不是暗示结果是完整的。若某次列举撞到了深度上限或遇到未解析的引用，
  那就是这次回答真正的边界，而不是一个需要绕开的缺陷。
- **调用边是下界。** 解析器无法解析的调用（动态分发、无类型接收者）不在其中，
  并且会这样说明。

---

## 支持的语言与资源

解析器是**沙箱化的 wasm 插件**，由 `drsg plugin install` 从官方目录安装，
插件只能触达三个宿主函数，根目录锁定在被消化的那个目录。

- **语言（8 种官方插件）**：Go、TypeScript/JavaScript、Python、Rust、Java、
  Kotlin、C#、C
- **运维**：Kubernetes 清单（含 Helm 渲染结果）、Docker Compose、Terraform
- **其他**：TOML、XML、Web、依赖锁文件、git 历史

git 历史本身也入图：每个仓库有两个平面，一个是当前代码，另一个是提交、分支、
标签、合并与 rebase 构成的历史——可以问「这次改动何时落地」「某个发行版包含什么」。

---

## 安全与隐私

- **完全本地。** 消化与回答全程不调用任何模型，代码不出网。
- **解析器在 wasm 沙箱内。** 无文件系统、无套接字，有燃料与时限。
- **服务默认只听回环地址。** 设置 `DRSG_TOKEN` 后对所有客户端启用 Bearer 鉴权；
  未设置时只应答回环 `Host`。
- **开源可审计。** MIT 或 Apache-2.0 双许可。

---

## 常见问题

**`drsg-mcp: command not found`**
npm 包与 `cargo install drsg` 都只含 `drsg`。stdio 接入需要
`--bin all` 的安装脚本，或 `cargo install dr-strange-mcp`。

**提示数据库不存在**
MCP 服务器从不创建数据库。先在仓库里 `drsg init`，或
`drsg digest <dir> --apply --db <path>`。

**Web 控制台是一张占位页**
`cargo install` 从源码编译，而该过程没有 bun，所以控制台没有被构建进去。
CLI、MCP 与 JSON-RPC 后端都是完整的。需要控制台就用安装脚本或 npm 包。

**图跟不上代码**
用 `drsg serve watch`（`drsg init` 默认就是这样起的），每次提交的改动文件
都会被重新解析、平面就地更新。要监视别处的仓库，用 `--dir <path>`。

---

## 链接

- 仓库：https://github.com/wangyingsm/dr-strange
- 文档（中文）：仓库内 `docs/zh`
- 基准方法与数据：仓库内 `BENCHMARKS.md`、`AGENT-BENCHMARKS.md`
- 解析器插件：https://github.com/wangyingsm/dr-strange-extension
- 许可证：MIT OR Apache-2.0
