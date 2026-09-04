# 快速上手

本章介绍如何安装 Dr Strange（用发行版二进制或从源码构建均可）、如何初始化数据库、
在命令行中发起查询，以及如何运行服务，包括本地运行和以容器镜像方式部署。

## 环境准备

安装发行版二进制除 `curl`（Windows 上为 PowerShell）之外别无所需。以下准备仅适用于
其他方式：

- 从源码构建：当前的 **Rust 工具链**（stable 通道），经 [rustup](https://rustup.rs) 安装。
- Web 仪表盘：**[bun](https://bun.sh)** 编译单页应用，以及可选的
  **[just](https://github.com/casey/just)** 作为任务运行器。
- 容器工作流：**Docker**（Engine 24+；仅在自行构建镜像时才需要 BuildKit）。

构建通过 rustls/ring 链接 TLS，无需 OpenSSL 工具链。

## 安装发行版二进制

每个打了标签的发行版都会发布 Linux、macOS 与 Windows 的二进制。安装脚本会选取与宿主
平台匹配的归档包，校验其发布的 SHA-256，并将二进制置入 `PATH`。可安装的二进制有两个：
命令行工具与服务端 `drsg`，以及面向 LLM 智能体的 MCP 服务 `drsg-mcp`（[第 8 章](./mcp.md)）。

**Linux**

```console
# 命令行与服务端 —— drsg
$ curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh

# MCP 服务 —— drsg-mcp
$ curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh -s -- --bin drsg-mcp
```

**macOS**（脚本相同，Apple 芯片与 Intel 版本均有发布）

```console
# 命令行与服务端 —— drsg
$ curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh

# MCP 服务 —— drsg-mcp
$ curl -fsSL https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh | sh -s -- --bin drsg-mcp
```

**Windows**，在 PowerShell 中执行。第二种写法将脚本作为代码块运行，因为以管道送入
`iex` 的脚本无法接收参数。

```console
# 命令行与服务端 —— drsg
PS> irm https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.ps1 | iex

# MCP 服务 —— drsg-mcp
PS> & ([scriptblock]::Create((irm https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.ps1))) -Bin drsg-mcp
```

三个选项可调整安装行为，每个都有对应的环境变量，便于在非交互场景中使用：

| 选项（Windows） | 环境变量 | 作用 |
|---|---|---|
| `--bin drsg-mcp`（`-Bin`） | `DRSG_INSTALL_BIN` | 安装哪个二进制——`drsg`（默认）、`drsg-mcp` 或 `all` |
| `--version v1.1.0`（`-Version`） | `DRSG_VERSION` | 指定某个发行版本，而非最新版 |
| `--dir <path>`（`-Dir`） | `DRSG_INSTALL_DIR` | 安装目录 |

安装目录默认为 `~/.local/bin`，Windows 上为 `%LOCALAPPDATA%\Programs\drsg\bin`，且脚本
会将该目录加入用户 `PATH`。在 Linux 与 macOS 上，若该目录尚不在 `PATH` 中，请将其写入
shell 配置：

```console
$ export PATH="$HOME/.local/bin:$PATH"
```

### 升级

`drsg update` 用与安装脚本相同的方式解析最新发行版本——走 `releases/latest` 的
重定向，而不是对未认证调用者有速率限制的 API——再与当前运行的构建比较。无事可做
时它会明说并停下：

```console
$ drsg update
drsg 2.4.1 is the latest release — nothing to do
```

确有新版本时，它会先打印即将执行的命令，然后**变成**它：进程被首次安装所用的同一
个安装脚本替换，因此退出码就是安装脚本自己的退出码，也不会留下一个父进程等在一个
刚刚被覆盖掉的二进制里。

```console
$ drsg update
drsg 2.4.0 -> 2.4.1
$ curl -fsSL .../install.sh | sh -s -- --bin drsg --dir '/home/me/.local/bin'
Dr Strange v2.4.1 (x86_64-unknown-linux-gnu)
  downloading dr-strange-v2.4.1-x86_64-unknown-linux-gnu.tar.gz
  checksum verified
  installed /home/me/.local/bin/drsg
```

安装目录取的是正在运行的这个二进制所在的目录，而不是安装脚本的默认值——升级必须
替换 `PATH` 上的那一份，而不是在别处放一份更新的、让旧的继续被运行。若 `drsg`
装在无写权限的位置，用 `--dir` 覆盖。同一目录下若装有 `drsg-mcp`，它会随 `drsg`
一并更新，无需另行指定——两个二进制属于同一个发行版本，若智能体宿主拿上一版的
服务端去对接这一版的 `drsg`，没有任何东西会提醒它。不想这样时，用 `--bin` 指明
要更新的对象：`drsg`、`drsg-mcp` 或 `all`。

比最新发行版本**更新**的构建——来自源码，或来自最后一个标签之后的分支——会被告知
自己在前面，不会安装任何东西：`update` 从不回退。在 Windows 上则什么都不会运行，
因为正在运行的可执行文件被锁定、无法被覆盖；它转而打印出可在新终端里粘贴执行的
命令。

归档包及其校验和也可以从[发行页](https://github.com/wangyingsm/dr-strange/releases)
直接下载。安装脚本只是对同一批产物的便捷封装，两个脚本都放在
[`scripts/`](https://github.com/wangyingsm/dr-strange/tree/master/scripts)，可以在
使用前自行查看。

## 从源码构建

Dr Strange 是一个 Cargo 工作区。编译命令行二进制 `drsg`：

```console
$ cargo build --release -p dr-strange-cli
```

产物为 `target/release/drsg`。默认构建选用原生 LSM 存储引擎；遗留的 redb 后端仍可
通过特性开关启用。

Web 仪表盘由 web crate 的构建脚本在编译期嵌入二进制。若要嵌入编译好的仪表盘而非占位
页，请在构建二进制之前先编译单页应用：

```console
$ just web-build          # bun install && vite build
$ cargo build --release -p dr-strange-cli
```

面向 LLM 智能体的 **MCP 服务**（[第 8 章](./mcp.md)）是一个独立的二进制 `drsg-mcp`：

```console
$ cargo build --release -p dr-strange-mcp
```

产物为 `target/release/drsg-mcp`。将其置于 `PATH` 中，或在宿主配置里以绝对路径引用它。

## 磁盘布局

`--db` 参数用于选择数据库路径。在原生后端下，数据库是一个**目录**，里面存放着
预写日志（WAL）与有序的 SST 文件，此外还有两个存放检索索引的伴生文件：

```text
graph.drsg/          数据库（WAL + SST 文件）
graph.drsg.hnsw      向量索引伴生文件
graph.drsg.bm25      关键词索引伴生文件
```

数据库在首次访问时创建，无需单独的初始化步骤。

## 创建一张图

先创建一个平面，再插入数据。openCypher 子集会编译为引擎直接执行的同一个逻辑计划：

```console
$ drsg --db graph.drsg plane create social

$ drsg --db graph.drsg cypher --plane social \
    'CREATE (a:Person {name:"Ada"}),
            (b:Person {name:"Alan"}),
            (a)-[:KNOWS]->(b)'
```

查询结果：

```console
$ drsg --db graph.drsg cypher --plane social \
    'MATCH (p:Person)-[:KNOWS]->(q:Person) RETURN q'
```

检视所得数据的形态与软 schema：

```console
$ drsg --db graph.drsg stats
$ drsg --db graph.drsg catalog --plane social
```

## 向量索引与相似度检索

向量是普通的属性值。在某个 `(标签, 属性)` 对上声明索引，然后对其发起查询。对文本
查询做嵌入是在服务端执行的，因此进程环境中需要一个提供方密钥（例如
`OPENAI_API_KEY`）。针对字面量向量的查询则不需要任何提供方。

```console
$ drsg --db graph.drsg index ensure Doc embedding --plane social

$ OPENAI_API_KEY=… drsg --db graph.drsg cypher --plane social \
    'SEARCH (d:Doc) ON embedding NEAR "a friendly greeting" TOPK 5 RETURN d'
```

由于结果均为图节点，可从其继续遍历——即第 1 章介绍的 GraphRAG 模式。

## 运行服务

```console
$ drsg --db graph.drsg serve
```

此命令启动 JSON-RPC 2.0 接口、WebSocket 变更流以及内嵌的仪表盘，并报告所绑定的
地址（默认 `127.0.0.1:7700`）。

**鉴权。** 未配置令牌时，只有同源的浏览器界面可以调用接口。若要允许来自 SDK 或
`curl` 的程序化访问，请配置一个共享令牌，并以 Bearer 凭据形式携带：

```console
$ DRSG_TOKEN=please-change-me drsg --db graph.drsg serve
```

### 配置文件

服务、日志与提供方设置也可以通过一个 TOML 配置文件提供，而非逐个使用命令行标志与
环境变量。配置文件的解析顺序为：`--config <路径>`，其次 `$DRSG_CONFIG`，再次为
当前目录下的 `./drsg.toml`（若存在）。系统会拒绝未知的键。

```toml
[server]
addr = "0.0.0.0:7700"                       # 监听地址（命令行 --addr 覆盖此项）
token = "please-change-me"                  # 共享 API 令牌（→ DRSG_TOKEN）
max_concurrent = 256                        # 并发请求上限
source_root = "/srv/myrepo"                 # grep/snippet 智能体工具读取的源码树（serve watch 会用 --dir 设置它）
allowed_origins = ["https://app.example.com"]  # 额外允许的浏览器来源

[server.tls]                                # 存在此节 ⇒ 以 HTTPS 提供服务
cert = "/etc/drsg/cert.pem"                 # PEM 证书链
key  = "/etc/drsg/key.pem"                  # PEM 私钥

[logging]
dir = "/var/log/drsg"                       # 滚动日志文件所在目录

[llm]                                       # 提供方密钥，导出至进程环境
OPENAI_API_KEY = "sk-…"
DEEPSEEK_API_KEY = "…"
DASHSCOPE_API_KEY = "…"

[digest]                                    # 服务端 AIgest 调优
concurrency = 8                             # 并发进行的分块抽取调用数
chunk_chars = 4000                          # 目标分块大小
embed_provider = "openai"                   # search / write_nodes / watch 重新向量化所用的嵌入提供方
embed_model = "text-embedding-3-small"      # 其模型（各提供方均有默认值）
embed_key_env = "OPENAI_API_KEY"            # 存放密钥的环境变量

[plugins]                                   # 预处理沙箱调优（均可省略）
fuel = 200000000000                         # 每次沙箱调用的指令预算（0 为不设限）
memory_mb = 3072                            # 每次调用的线性内存上限，MiB；按照 wasm32 标准，最高支持 4096

[fetch]                                     # URL 导入（第 3 章）
enabled = true                              # 置为 false 则彻底拒绝 URL 抓取
max_pages = 10                              # 单次爬取保留的页数上限
max_depth = 3                               # 请求可申请的沿链接深度上限
concurrency = 4                             # 并发请求数
allow_private = []                          # 见下文——通常保持为空
```

**`[fetch]` 改变的是服务端的网络姿态**，在启用其中任何一项之前值得先读一读。当 URL
导入处于开启状态（默认如此）时，客户端可以指定一个地址，而后由**服务端**去连接它。
服务端在网络中所处的位置通常更为特权，因此系统会拒绝一切不可路由的地址：回环地址、
RFC-1918 私有网段、链路本地地址（`169.254.0.0/16`，云实例元数据服务在此应答凭据）
等等。检查针对的是**解析之后的地址**而非主机名，并在每一次重定向跳转时重新执行。

`allow_private` 用来重新放行特定的 CIDR 网段（例如设为 `["10.0.0.0/8"]`，以便
读取内网 wiki）。它是上述规则唯一经过深思的例外，而**不是**关闭这层防护的开关：
暴露给不受信任客户端的服务端应当让它保持为空。若要完全拒绝 URL 抓取，请设置
`enabled = false`。

优先级是固定的：进程中已设置的环境变量始终优先于配置文件中对应的取值，而 `--addr`
标志覆盖 `[server].addr`。提供 `[server.tls]` 会将服务切换为 HTTPS。

## 只读副本（`serve --follow`）

以 `--follow` 启动的第二个 `drsg serve` 会以只读方式镜像一个正在运行的实例——
用于在一组智能体之间扩展读取，而无需让每一次查询都挤过同一个进程：

```console
$ drsg --db replica.drsg serve --addr 127.0.0.1:7701 \
    --follow ws://master-host:7700 --follow-token please-change-me
```

无论令牌是否有效，任何写入 RPC 都会被拒绝。启动时——以及此后每次断线重连
时——副本都会从主库的 `/snapshot` 端点拉取一份完整、一致的快照，随后跟随其
`/ws/wal` 获取后续提交；每次重连都会从零开始重新同步（arch/01 §9），因此
`--db` 必须指向一个空目录，或指向这个副本自己曾经使用过的目录（由一个
`.drsg-follower` 标记文件记录）——除此之外的目录会被拒绝，而不是被静默清空。

`--follow-token`（或 `DRSG_FOLLOW_TOKEN`）是提交给主库的凭据，与这个副本自己
用来约束其下游客户端的 `DRSG_TOKEN` 相互独立。当主库的地址不是回环地址时，
`--follow` 应当指向 `wss://` 而非 `ws://`——否则承载令牌的连接会以明文传输，
这与 `[server.tls]` 对入站连接的要求是同一个道理。

## 容器镜像

一个多架构镜像（`linux/amd64` 与 `linux/arm64`）已发布至 GitHub 容器镜像仓库
（GHCR）。直接拉取并运行，无需构建：

```console
$ docker run -p 7700:7700 -v drsg-data:/data \
    -e DRSG_TOKEN=please-change-me \
    ghcr.io/wangyingsm/dr-strange:latest
```

`docker run` 会在首次使用时拉取镜像。若需可复现的部署，请以版本标签
`ghcr.io/wangyingsm/dr-strange:1.0.2` 替代 `:latest`。运行时镜像绑定到
`0.0.0.0:7700`，数据库存放在 `/data` 卷中（原生后端下数据库是一个目录，这个卷
负责把它持久化保存）。提供方密钥通过环境变量传入。

对于持久化部署，`docker-compose.yml` 拉取同一镜像并定义了一个具名卷：

```console
$ DRSG_TOKEN=please-change-me docker compose up
```

若想改为在本地构建镜像，仓库提供了一个多阶段 `Dockerfile`：它会编译仪表盘、将其
内嵌进二进制，并产出一个精简的运行时镜像：`docker build -t dr-strange:latest .`。

## 下一步

- **第 3 章 —— AI 原生：** 嵌入、混合检索、自然语言查询与文档导入。
- **第 4 章 —— 查询语言：** openCypher 子集及其底层的逻辑计划。
- 按访问方式：**第 6 章 —— SDK**（应用代码）、**第 7 章 —— 嵌入式命令行**（运维）、
  **第 8 章 —— MCP**（LLM 智能体）。
