# 编码智能体

[上一章](./plugins.md)讲了解析器插件如何把一个代码仓库变成事实。这一章讲这些
事实能为编码智能体带来什么，以及如何为官方目录尚未覆盖的语言构建你自己的插件。

四条命令，就能把一个代码仓库变成可查询、随提交同步的图：

```console
# 安装解析器插件：可以给出官方目录中的名字；不带参数时打开该目录的交互式
# 选择器（0 = 全部）；也可以直接给出任意 .wasm 路径或 URL。
$ drsg plugin install

# 将一个代码仓库图化为一个以其命名的平面
$ drsg --db codes.drsg digest ~/src/myrepo --apply --no-embed

# 启动 API + MCP 服务，并让平面随每次提交保持同步
$ drsg --db codes.drsg serve watch --dir ~/src/myrepo

# 一个符号的完整邻域，一次调用
$ drsg --db codes.drsg context 'WriteTxn::delete_node' --plane myrepo
```

`--no-embed` 跳过向量嵌入，因为解析本身不需要任何模型。之后运行 `drsg vectorize`，
平面就能支持语义检索。

**`drsg init`** 把图化和启动服务这两步合并成一条命令。用它之前插件要先装好；在
代码仓库自身目录下运行后，它会把工作目录图化为一个以其命名的平面，在后台启动一个
`serve watch` 进程（监听地址随机选定，令牌也随机生成），并写入 `.mcp.json`。这是
Claude Code 自己的约定，GitHub Copilot 也会原样读取它。随后，它还会为 Cursor、
OpenCode、Gemini CLI、Codex CLI、Trae 或 Kimi Code 各自写入一份匹配的 MCP
配置——但只有这个工具自己的标记（它会创建的目录，或者已经拥有的配置文件）已经
出现在这个仓库里、或者出现在这台机器上，才会写入。Qoder 与 CodeBuddy 不需要
额外的文件：两者都读取项目自己的 `.mcp.json`，所以 `init` 只把这一点说出来，
而不是再写一份。

令牌写在哪里，取决于每个客户端能做什么。`.mcp.json`、`.cursor/mcp.json` 和
`.trae/mcp.json` 直接携带令牌——Cursor 和 Trae 都是从桌面启动的 IDE，没有
shell 环境可供读取——这三个文件都会进入 `init` 维护的 `.gitignore` 段落。
`.opencode.json` 与 `.gemini/settings.json` 在 `init` 之前就存在，很可能已被
提交，因此它们改为按各自客户端的语法写入一个环境变量引用（`{env:DRSG_TOKEN}`、
`$DRSG_TOKEN`），正如 Codex 条目一直采用的 `bearer_token_env_var`，以及
Kimi Code 通过写下变量名的 `bearerTokenEnvVar`；`init` 会打印出在这里启动这些
客户端之前需要执行的 `export DRSG_TOKEN=$(drsg token --unhide)`。这一行从
`.mcp.json` 里读出令牌，而不是把令牌本身打印出来，因为 `init` 的输出会留在终端回滚、
CI 日志和智能体的对话记录里。负责读回令牌的是 `drsg token`，因此这一行不依赖任何额外
安装的工具；当这个目录尚未运行过 `init` 时，它也会直接说明该怎么做。

**`drsg token` 默认只打印遮蔽后的形式**——`AAAA…LmXw <40 characters>`，足以分辨不同
仓库持有的是哪一个令牌，又不足以拿去使用。`--unhide` 才会打印令牌本身，专供上面那种
命令替换使用：此时令牌直接进入环境变量，不会出现在屏幕上。若令牌太短，两端字符就会
泄露过多，这时会被整体遮蔽。

Trae 把项目级 MCP 放在 **Settings > Beta > Enable Project MCP** 之后，没有打开
这个开关时，`init` 写入的文件不会生效；`init` 打印的那一行会提醒这一点。

对 Claude Code，它还会安装两个钩子：写在仓库的 `.claude/settings.local.json`
（每个用户自己的文件，不会进入团队共享的代码树）里，脚本放在插件库旁边。配置
告诉智能体图在哪里；钩子告诉它*何时*去用。`SessionStart` 钩子把规则放进智能体
的上下文——每个代码问题先问 drsg，shell 是最后手段——恢复会话和压缩上下文之后
它仍然在。`PreToolUse` 钩子在智能体对代码执行 `rg`、`grep`、`cat` 或 `sed -n`
的那一刻，改为指出能回答这个问题的动词；`DRSG_RAW=1 <command>` 可以原样执行任何
命令，留给没有任何平面覆盖的文件。其他宿主没有钩子机制，它们从服务端自身的 MCP
instructions 里得到同一条规则——每个宿主都会把它放进系统提示。

```console
$ drsg init
plane 'myrepo' bootstrapped — serve watch pid 48213, http://127.0.0.1:51900/mcp
  + wrote ./.mcp.json
  + Claude Code: hooks in ./.claude/settings.local.json — a shell search or read on code is redirected to the drsg tools (DRSG_RAW=1 <command> runs it anyway)
  + Cursor: wrote ./.cursor/mcp.json
  + Gemini CLI: wrote ./.gemini/settings.json (no token inside it — export DRSG_TOKEN=$(drsg token --unhide) before launching `gemini` here)
```

令牌的去向取决于各客户端能做什么：`.mcp.json` 与 `.cursor/mcp.json` 直接写入令牌
（Cursor 是桌面应用，启动时没有可读取的 shell 环境），并被加入 `init` 维护的
`.gitignore` 块；`.opencode.json` 与 `.gemini/settings.json` 在 `init` 之前就已存在、
多半已提交，因此只写入该客户端自身语法的环境变量引用（`{env:DRSG_TOKEN}`、
`$DRSG_TOKEN`），与 Codex 条目一样——`init` 会打印启动这些客户端前要执行的
`export DRSG_TOKEN=…`。

**服务进程没了，就再运行一次 `drsg init`。** 它在后台启动 `serve watch`，并把这个
进程的地址与令牌记录在 `.mcp.json` 里；但没有任何东西会重启它——MCP 的 `http` 条目
告诉客户端去哪里连接，而不是去启动什么，所以没有哪个智能体会把它拉起来，它也不会在
重启、崩溃或被杀掉之后存活。重新运行 `init` 就是恢复的办法，而且任何时候运行都是
安全的：它会先探测记录下来的端点（`GET /health`，服务端正是为此让这个端点免鉴权），
服务还活着就原样保留、连数据库都不打开，服务已经死了则用**同一个**地址和令牌重启，
并且不重新解析源码树——平面会从它记录的提交继续。

```console
$ drsg init                       # 已在运行：无事可做
drsg is already serving . at http://127.0.0.1:51900/mcp — reusing it, the plane is untouched

$ drsg init                       # 重启机器之后：地址与令牌都不变
plane 'myrepo' restarted — serve watch pid 51002, http://127.0.0.1:51900/mcp
```

复用这条路径上不打开数据库，正是它可以安全地对着一个已在运行的服务执行的原因：一个
平面同一时刻只允许一个进程打开，若 `init` 为了检查而去打开它，那本身就成了它要避免的
那次冲突。重启前后每个智能体的配置都依然有效，因为地址和令牌是从 `.mcp.json` 里恢复
的，而不是重新生成的；如果那个记录下来的端口此时已被别的进程占用，`init` 会换一个空闲
端口并明确说明。想让端点彻底固定下来，就在 `drsg.toml` 的 `[server]` 下写死 `addr`
与 `token`。

**`--rebuild` 会丢弃平面并重新读取整棵源码树。** 一个源文件意味着什么，是由解析器
插件决定的；因此升级过的插件只有在平面重新经它折叠之后才会体现出来——续跑的平面对
那些此前已经看过的文件，保留的仍是旧解析器的理解。`--rebuild` 会先停掉持有数据库的
服务进程（它同时也持有每个插件的旧副本），从整棵树重建，然后在同一地址、同一令牌上
回来，因此任何智能体的配置都不会在脚下发生变化：

```console
$ drsg plugin install rust        # 更新版本的解析器
$ drsg init --rebuild
stopping serve watch pid 51002 — the plane is about to be rebuilt
plane 'myrepo' rebuilt — serve watch pid 51188, http://127.0.0.1:51900/mcp
```

一如既往，只重建事实：嵌入向量会在下一次 `drsg digest` 时回来。

所以：每个仓库运行一次，此后只要服务没了就再运行一次；而当某个插件改变了源码树的
含义时，就带上 `--rebuild` 再运行一次。

## 事实为智能体带来什么

依赖 grep 工作的智能体，每个问题都要重建一遍结构：搜索、打开文件、阅读、推断谁
调用了什么，再来一遍。已图化的平面把这份工作提前做完了，只做一次，由解析器完成。
于是结构性问题就变成了**一次往返**，而不是一轮搜索加阅读的循环。

十二个动词承担主要工作，在 MCP（[第 8 章](./mcp.md)）与 CLI（[第 7 章](./embedded-cli.md)）
上完全一致——`grep`、`snippet` 与 `recall` 读取平面被解析自的那棵源码树，平面若未
记录，则由 `--root` 指明：

| 动词 | 它回答的问题 |
|---|---|
| `context` | 关于一个符号的一切——定义、带调用位置的调用者、被调用者、引用——首选动词 |
| `search` | “我不知道名字”：在平面的向量嵌入上做语义 top-k |
| `describe` | 一个符号的属性——只看节点的轻量视图 |
| `grep` | 在被监视的源码树上做文本检索——字面或正则，可按路径限定、带上下文行，每个命中标出所在符号；有界且带计数 |
| `trace` | 一个符号如何到达另一个：图中记录的最短调用路径 |
| `impact` | 影响范围：所有能到达该符号的东西，按距离分组——加上 `--across` 后，还包括这个半径触及了哪些其他仓库 |
| `fathom` | 一个符号身处怎样的地方：几跳之内的区域，按标签与边类型计数，并给出枢纽 |
| `snippet` | 一个符号的源码文本，或文件的某个行区间（`path:start-end`）——智能体不再需要的那条 `sed -n` |
| `recall` | 某次提交时代码的样子——一个文件、一个符号当时的声明、那棵树上的检索，或两个修订版本之间的差异——智能体不再需要的那条 `git show` 与 `git log -p` |
| `history` | 代码背后的仓库：HEAD 在哪里、分支与标签指向什么、哪些被变基过，以及最新的提交 |
| `deployed` | 某个运行中的东西是由哪次提交构建的，以及哪些东西在跑某次提交——把清单构成的运维平面连到它旁边的历史平面 |
| `wire` | 什么在提供某个 HTTP 路径、什么在请求它——跨平面内的每种语言匹配，也跨它所附着的平面 |

`grep` 与 `snippet` 只打开它们所属那棵树之内的文件——服务端所监视的目录，或某个
平面被摄取时所依据的那个目录——并拒绝任何离开它的路径（`..`、绝对路径、指向检出
之外的符号链接）；stdio 与共享服务端在这一点上的差别见[第 8 章](./mcp.md)。

每个回答都是紧凑的、每行一条事实的文本，尺寸是按模型的上下文窗口定的，而不是按
终端。`context` 会收紧各分组的条数上限，把自己保持在固定预算之内，并写明省略了
什么。

在 `serve watch` 之下，图会跟踪每一次提交。变更过的文件重新经过插件，平面随之
就地更新：新符号被创建，消失的符号被删除，边被改写，最终收敛到和一次全量重新
图化完全相同的结果。每个回答都以 `synced: commit <sha>` 开头，智能体因此知道
自己推理的是*哪一版*代码。工作区里未提交的修改，在真正提交之前都不可见，而回答
会通过注明提交号把这一点说清楚。

历史是另一半。对 git 检出目录的图化还会写入 `<name>_git`，而 `recall` 能读出该
平面所记录的任意一次提交时的代码——按 sha、分支、标签、日期或 `HEAD~n` 指定——
于是“重构之前这个函数是什么样子”只需一次调用，无需检出旧版本。符号的位置由当时
的文件重新解析得出，而不是沿用今天的行号；那次提交中并未声明的符号会被如实报告为
不存在，并列出该文件当时声明了什么。

运维是第三个平面。对 Compose 文件、Kubernetes 清单或 Terraform 配置的图化会写入
`<name>_ops`——工作负载、容器、它们运行的镜像，以及底下那一层基础设施——而
`deployed` 通过两端都会指名的那一个字符串，也就是镜像的标签，把它连到历史上。
只要流水线把修订号写进了那个标签，**“这个服务跑的是哪次提交”**就只是一次调用。
若没有写进去，答案会说清是哪一种原因——浮动标签、只锁定字节而不说明来历的
digest、这里的历史并不持有的修订号——因为一个对这三种情况都回答“什么都没有”的
动词，会让读者无法区分缺口与事实。平面是硬命名空间，所以这是对两个平面的一次
读取，而不是它们之间的一条边。

## 路由：图不再止于一个仓库的地方

一条调用边需要解析器看得见的被调用方，而一次 HTTP 请求没有：页面调用
`/api/v1/orders/42`，处理它的却是另一种语言写的控制器，往往还在另一个仓库里，
两者之间只有一个字符串。这样的图里两个文件都在，却没有任何连接。

每种语言声明路由的写法各不相同——带路径的注解（`@GetMapping`、`@app.get`、
`#[get]`）、以方法名表示动词的注册调用（`app.get('/x', h)`、`r.GET`、
`http.HandleFunc`），或是带路径参数的客户端调用。插件把这三种写法统一记录到
本就存在的那个符号上：声明记为 **`serves`**，调用记为 **`requests`**，都是
`VERB /path/{}` 的形式，每个可变段都归约为 `{}`。目前 Go、Java、Python、Rust
与 TypeScript 插件会写入它们。

图中不会新画任何东西。没有 `Route` 节点，也没有 `SERVES` 边——合成出来的边会把
一个猜测固化下来，而运行时拼出的路径正是猜测。`wire` 在每次调用时计算这一对应
关系，并报告它无法匹配的部分：到不了任何声明的请求、没有人请求的声明（一个死端点，
这本身就有价值），以及源码中从未声明的前缀。未写出动词的一侧会匹配该路径上的
所有动词，**并且会把这一点说出来**。

`attach` 是另一半。前端与后端是分开的仓库，因此也是分开的平面；一条附着记录下
把它们连起来的那一条事实——*本平面可以询问那个平面*。它自身不携带任何词汇：问题
是什么、两侧按什么键匹配，都属于提问的那个动词——因此任何需要跨图的动词读的都是
同一条记录。

```console
$ drsg attach fe --plane be --both     # 本实例上的两个平面，双向
$ drsg attach front --plane be --url http://127.0.0.1:36500 --as fe
$ drsg attachments --plane be --probe  # 它们现在各自是否应答？
```

本实例上的平面直接读取。由另一个 drsg 提供服务的平面通过 `plane.query` 读取——
那是每个 drsg 早已提供的读取方法——因此跨实例既不新增协议，也不新增 SDK 方法；
联系不上的附着会连同原因一起列出，而不是被跳过。每一行都会标明它来自哪张图，
因此跨越两个仓库的回答仍然说得清每条事实住在哪里。

> **“附着”的两种含义。** 源码树处于同步状态的平面也被称为已附着
> （`serve watch`、`synced_root`），那指的是一个平面背后的那棵树；而这里的附着
> 指的是本平面可以询问的另一个平面。两者互不相关。

## 一个公共库，以及依赖它的那些仓库

「改这个类会影响哪些业务仓？」问的正是 `impact` 早已回答的那个问题，只是往外
挪了一个仓库。公共库里一个类的改动是局部的，它的后果却不是；而在单个平面里走
出来的半径看不见那些后果。

`impact --across` 把这个问题也在其他图里回答，依据的是**两侧本就已经陈述了的
两个键**——没有新提取任何东西，也没有任何插件需要改动：

| 连接 | 本平面持有 | 依赖方持有 |
|---|---|---|
| **构建** | `Manifest.declares`——它自己的 manifest 所发布的坐标 | 一个以该坐标为键的节点，`version` 与 `scope` 记在 `DEPENDS_ON` 边上 |
| **代码** | 被改动的符号，以及它半径内的每一个键 | 同一个键的替身，带 `External` 标签 |

坐标是*从本平面读出来*的，绝不是映射上去的。一个 Maven 坐标
（`com.acme:common-base`）与一个 Java 包（`com.acme.common…`）之间，不猜就无法
互相映射——这正是 `deps` 插件记录构建依赖、却拒绝把它和任何 import 连起来的原因。
而站在这个库里就无需去猜：它自己的 `pom.xml` 说明了它发布哪个坐标，而依赖方的
`pom.xml` 给它的声明用的是同一个键。

```console
$ drsg impact com.acme.common.BaseEntity --plane commonlib --across all
com.acme.common.BaseEntity  Class  src/main/java/com/acme/common/BaseEntity.java:3-6
depth 1 (1):
  com.acme.common.Dated  Class  src/main/java/com/acme/common/Dated.java:3-5  [EXTENDS]
total affected within depth 3: 1
note: recorded call edges only — calls the parser could not resolve (dynamic dispatch, untyped receivers) are absent, so this is a lower bound.

across  commonlib, svcorders, svcbilling
in svcorders
  declares  com.acme:common-base  2.4.1  [runtime]  pom.xml
  com.acme.common.Dated  ← 1
    com.acme.orders.Order  Class  src/main/java/com/acme/orders/Order.java:5-8  [EXTENDS]
in svcbilling
  declares  com.acme:common-base  2.3.0  [runtime]  pom.xml
2 graph(s) read, 2 hold(s) something: 2 declare(s) a dependency, 1 reach(es) it in code
1 of the 2 key(s) offered are referenced elsewhere
```

`svcorders` 是经由 `Dated` 而非被改动的那个类本身被触及的——那是半径所持有的一
个键，而不只是它的种子。`svcbilling` 声明的依赖停在一个更旧的版本上，并且完全
没有用到它，这同样是一个值得拿到的回答。

两种范围，因为有两种环境以不同方式提出这个问题。只写 `--across` 读的是本平面所
附着的那些平面：显式，而且这是唯一能到达另一个 drsg 的范围。`--across all` 读的
是本实例持有的每一个平面——于是一台被交付了三十个仓库的机器无需任何配置就能被跨
仓追问；空平面与 `…_git` 历史平面不在其中。两种范围都会说明自己读了哪些图，以及
哪些读不到。

不写 `--across` 时，回答逐字节不变。`wire` 无需被要求就跨图，因为一条路由的两半
天然住在不同的仓库里；而一个半径在单个仓库之内**本就是完整的**——加入更多图会改
变那个总数的含义，所以由调用方来说要看多远。

由另一个 drsg 提供服务的图回答的是节点，因为 `plane.query` 返回的就是节点，而版本
是一条关于边的事实。于是它报告自己持有哪些键——这本身就是「它内部有东西引用了这些
键」这句陈述，因为替身只会在有引用把它铸出来的地方存在——并指出读取其余部分的那次
调用：

```console
across  commonlib, far
in far  at http://127.0.0.1:36600
  declares  com.acme:common-base  (the manifest and its version are read there)
  com.acme.common.Dated  ← referenced; what references it is read there
  ask that drsg for the rest: `impact <key> --plane svcorders`
```

它还守住了另一条区分：持有某个键却**不带** `External` 的图并不是依赖方，而是同一份
代码的又一份——被 vendor 进来，或者被图化了两次——它会被报告为 `holds its own`，而
不是被算作一个使用者。

## 诚实是基石

智能体会依据工具的回答直接行动，所以这个家族的规则——*错误的边比缺失的边更糟*——
塑造了每一个回答：

- **歧义的名字从不猜测。** 两个符号都叫 `delete_node`？回答是候选清单，用精确键
  重试只多花一次调用——比一个被信心十足地采纳的错误答案便宜得多。
- **调用清单是一个明示的下界。** 解析器没能解析的调用，会变成带原因的
  `UnresolvedRef` 条目，直接呈现在 `context` 里。于是"谁调用了它"这个问题，回答
  会带着未解析的残余一起返回，而不是被悄悄缩短。
- **图在回答之内点明自己的盲区**：诚实脚注、`synced:` 行、省略计数。智能体可以
  自行决定何时回落到 `grep`；`grep` 动词就在同一套工具里，回落只是多一次往返，
  不用换工具。

在与 ripgrep 工作流、以及两款开源代码图 MCP 工具的基准对比中，条件是一致的：
同样的语料、同样的任务，每个智能体只用一种工具。结果是，这套组合完成了每一种
任务形态（调用者、影响面、调用链、复合审计），每项任务只需 2–4 次工具调用，
边际 token 开销最低，而且是唯一一个会在回答中明示自身边界的工具。方法与完整
表格见
[AGENT-BENCHMARKS.md](https://github.com/wangyingsm/dr-strange/blob/master/AGENT-BENCHMARKS.md)。

## 为新语言构建插件

插件系统是开放的：依照 SDK 构建的社区解析器，与官方插件以完全相同的方式安装、在
完全相同的沙箱中运行，遵循同一份[契约](./plugins.md#契约)与同一条
[不得调用 LLM 的规则](./plugins.md#插件不得调用-llm这是规则)。每个官方插件都
遵循同一种模式，也是给新插件最值得借鉴的建议：包装一个**成熟的、最好是该语言
公认的解析器**（syn、swc、ruff、tree-sitter），而不是自己动手写一个；把解析器
保持成普通的原生库，外面只套一层薄薄的组件封装，这样它的测试完全不需要 wasm
工具链。

### Rust

依赖 SDK，实现两阶段契约；没有跨文件工作的格式也可以只实现单函数的简化接口：

```toml
[dependencies]
dr-strange-ext = { git = "https://github.com/wangyingsm/dr-strange-extension" }

[lib]
crate-type = ["cdylib"]
```

```rust
use dr_strange_ext::{Input, Manifest, Output, OutputExt, Simple, host, node, output, simple_plugin};

struct MyPlugin;

impl Simple for MyPlugin {
    fn describe() -> Manifest {
        Manifest { name: "mine".into(), version: "1".into(), extensions: vec!["xyz".into()] }
    }

    /// One subject at a time; the SDK derives parse/assemble from this.
    fn process(subject: Input, _options: &[(String, String)]) -> Result<Output, String> {
        let mut out = output();
        if let Input::Files(paths) = subject {
            for path in paths {
                let bytes = host::read(&path)?;
                out.nodes
                    .push(node(&path, "Thing").prop("bytes", bytes.len() as i64).build());
            }
        }
        Ok(out.finish())
    }
}

simple_plugin!(MyPlugin);
```

```console
$ cargo build --release --target wasm32-wasip2
$ drsg plugin install target/wasm32-wasip2/release/my_plugin.wasm
```

真正的语言解析器会直接实现生成出来的 `Guest` trait：`parse` 每块返回一个序列化
的部分结果，`assemble` 在全部部分结果之上做跨文件解析，还会通过宿主绑定拉取
相邻文件。官方的 `plugins/rust` 就是一个现成的完整范例。

### Go

实现 `ext.Plugin` 接口，用 TinyGo 构建（≥ 0.41，`wasm-tools` 需在 `PATH` 上）：

```go
package main

import ext "github.com/wangyingsm/dr-strange-extension/sdk/go"

type mine struct{}

func (mine) Describe() ext.Manifest {
    return ext.Manifest{Name: "mine", Version: "1", Extensions: []string{"xyz"}}
}

func (mine) Parse(subject ext.Subject, options map[string]string) ([]byte, error) {
    // Pull files via ext.List / ext.Read; serialize your partial.
    return []byte{}, nil
}

func (mine) Assemble(partials [][]byte, options map[string]string) (ext.Output, error) {
    return ext.Output{Nodes: []ext.Node{{Key: "k", Label: "Thing"}}}, nil
}

func init() { ext.Register(mine{}) }
func main() {}
```

```console
$ tinygo build -target=wasip2 -scheduler=none -gc=leaking \
    --wit-package ./wit --wit-world drsg:preprocess-build/plugin-go -o mine.wasm .
```

这些构建参数每一个都有各自的作用（扩展仓库的 `justfile` 解释了原因）。Go SDK
里贯穿一条规则：凡是从 ABI 里取出来的数据，要先复制再使用，因为 `cm` 切片只是
一个视图，垃圾回收器可能在你使用的时候把底下的数据挪走。

### 好解析器要保持什么

[家族承诺](./plugins.md#每个官方解析器的承诺)虽是约定，但智能体的信任建立在
它们之上，新插件应当逐条保持：用语言自己的全限定名作键、每条事实带 `file`/`line`、
每条边带解析印记、用未解析台账代替猜测、树外代码用 `External` 替身。用该语言
生态中的真实源码对解析器做原生测试。构建前运行 `just check-wit`，确保契约的受控
副本与规范副本一致。

想把它提供给所有人：贡献先从在[扩展仓库](https://github.com/wangyingsm/dr-strange-extension)
开一个 issue 开始，写明你打算包装的解析器，再以 `plugins/<名称>/{parser,component}`
的形式落地，并附带原生测试套件。CI 会在每次推送时构建所有组件、运行所有测试。
