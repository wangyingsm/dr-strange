# 嵌入式命令行

`drsg` 是命令行界面。它**直接**打开数据库——它是嵌入式的，而非某个正在运行的服务的
客户端——因而适合本地脚本化、批量导入、检视、备份与运维。每次调用都接受一个全局的
`--db <路径>`（默认 `graph.drsg`）；多数命令还接受一个 `--plane`（默认 `startup`）。

由于它在进程内打开数据库，`drsg` 无法对一个 `drsg serve`——或另一个 `drsg`——当前正持有
打开的数据库进行操作：数据库会取得一把排他锁，第二次打开将以明确的错误失败，而不是把它
弄坏。对于并发访问，请使用面向该服务的 SDK 或 RPC 接口，而将命令行用于离线操作。

## 命令参考

| 命令 | 用途 |
|---|---|
| `init` | 创建一个空数据库（多数命令也会在首次使用时创建） |
| `plane list \| create \| drop \| show` | 平面生命周期 |
| `import <文件> --plane` | 加载 JSONL 节点/边 |
| `export --plane` | 将一个平面导出为 JSONL |
| `get <节点> --plane` | 按 id 或 `@外部键` 获取一个节点 |
| `query <计划> --plane` | 运行一个序列化的逻辑计划（JSON，或 `-`） |
| `cypher <查询> --plane` | 运行一条 openCypher 子集语句（或 `-`） |
| `catalog [--plane]` | 打印软 schema（单个平面或整个数据库） |
| `algo <名称> --plane` | 运行一个图算法 |
| `hybrid <查询> --plane` | 融合的向量 + 关键词 + 图邻近度检索 |
| `index ensure \| keyword` | 声明一个向量或关键词索引 |
| `ask <问题> --plane` | 自然语言查询 |
| `digest <文件\|目录\|url> --plane [--mode]` | 导入一篇文档、一个代码仓库，或一个页面及其链接 |
| `search <查询> --plane` | 语义查找：嵌入查询文本，返回最近的节点 |
| `context \| describe \| trace \| impact <名称> --plane` | 已图化代码平面上的智能体动词 |
| `vectorize --plane` | 为一个平面的节点生成向量嵌入，以供相似度检索 |
| `plugin install \| list \| remove` | 管理预处理插件（沙箱化的 wasm 解析器） |
| `snapshot <out>` / `restore <in>` | 整库备份与恢复 |
| `stats` / `check` | 汇总计数 / 完整性扫描 |
| `serve [--addr]` | 运行 Web 仪表盘 + JSON-RPC 接口 + MCP 端点 |
| `serve watch [--dir]` | 对外服务，并让代码平面随每次提交保持同步 |

被消化的文件可以是 Markdown 或纯文本，也可以是 Word、PowerPoint、Excel、
OpenDocument、RTF、EPUB、CSV 与 PDF——后者会先转换为 Markdown，使模型读到的是标题、
表格与列表，而非散乱的字符。格式依据文件内容判断，因此扩展名写错也不影响。目录会被
逐文件遍历并分发：源码经由已安装的解析器插件成为事实，文档则作为散文交给模型。

全局选项为 `--db <路径>` 与 `--config <路径>`（配置文件，见
[第 2 章](./getting-started.md#配置文件)）。

## 平面

```console
$ drsg --db graph.drsg plane create social
$ drsg --db graph.drsg plane list
$ drsg --db graph.drsg plane show social
$ drsg --db graph.drsg plane drop social
```

## 数据进出

导入与导出使用 JSONL——每行一个节点或边——因此一个平面可经由文件系统往返，并便于与
其它工具集成：

```console
$ drsg --db graph.drsg import nodes.jsonl --plane social
$ drsg --db graph.drsg export --plane social > social.jsonl
$ drsg --db graph.drsg get @ada --plane social
```

节点的 `external_key` 是它在平面内的身份标识，因此 `--on-conflict` 决定了当导入的键
已经存在时的处理方式：

| 策略 | 效果 |
|---|---|
| `error`（默认） | 不导入任何内容，并列出发生冲突的键 |
| `skip` | 保留已有节点，丢弃传入的那一行 |
| `update` | 覆盖已有节点的属性；若该行带有 `labels`，一并覆盖标签 |

```console
$ drsg --db graph.drsg import nodes.jsonl --on-conflict update
imported 0 nodes, 0 edges, 2 existing updated
```

在 `skip` 与 `update` 下，文件中的边仍会解析到平面中已存在的节点，因此关系可以挂载到
既有数据上。边本身不带键，故总是追加写入——该策略约束的是节点身份，而非边的去重。
没有 `external_key` 的行不可能发生冲突，始终会被插入。

## 查询

一条语句既可以是 openCypher 子集，也可以是一个序列化的计划；两者均接受 `-` 以从标准
输入读取。`--param` 将一个 `$name` 占位符绑定到一个 JSON 值：

```console
$ drsg --db graph.drsg cypher --plane social \
    'MATCH (p:Person) WHERE p.age >= $min RETURN p' --param min=18

$ drsg --db graph.drsg query - --plane social < plan.json

$ drsg --db graph.drsg catalog --plane social
```

## 图算法

```console
$ drsg --db graph.drsg algo pagerank      --plane social --top 10
$ drsg --db graph.drsg algo components    --plane social
$ drsg --db graph.drsg algo shortest-path --plane social --src 1 --dst 42
$ drsg --db graph.drsg algo louvain       --plane social
```

## 检索与导入

声明索引、运行融合检索、以自然语言提问，并导入文档（[第 3 章](./ai-native.md)）。
由 LLM 支撑的命令从环境读取提供方密钥：

```console
$ drsg --db graph.drsg index ensure  Doc embedding --plane social
$ drsg --db graph.drsg index keyword Doc body      --plane social --lang english

$ drsg --db graph.drsg hybrid "how does time-travel work" \
    --plane social --label Doc --vector embedding --keyword body --graph-hops 1

$ drsg --db graph.drsg ask "which companies does Ada work for?" \
    --plane social --chat deepseek --embed qwen

$ drsg --db graph.drsg digest notes.md --plane social --apply

$ drsg --db graph.drsg digest paper.md --plane papers --mode super --apply

$ drsg --db graph.drsg digest https://example.com/paper --plane papers \\
    --topic "attention mechanism" --pages 6 --apply
```

以 `http(s)://` 开头的参数会被抓取，而非从磁盘读取：该页面被转换为 Markdown，并在
`--pages` / `--depth` 的范围内沿其链接继续抓取，保留与页面自身主题（以及 `--topic`，
若给出）相关的内容（[第 3 章](./ai-native.md#从-url-读取)）。协议头是必需的——裸的
`example.com` 也可以是一个合法的文件名，与其去猜使用者的本意，不如要求写明。该命令会
打印保留了什么、丢弃了什么；它不提供交互式的挑选，那是仪表盘页面列表的用武之地。

`--mode` 决定抽取结果被清理到何种程度：`coarse` 归一标签与边类型的词表，`fine`
（默认）在此之上合并指称同一事物的实体，`super` 再将每个实体与其全部段落一并重读
——最为准确，输入 token 用量约为 15 倍（[第 3 章](./ai-native.md#抽取精度)）。

## 备份与完整性

`snapshot` 在单一提交序号上写出一个一致的整库快照包；`restore` 将其重建进一个空
数据库，并保留 id、提交序号与已构建的检索索引（[第 9 章](./architecture.md)）。
`stats` 与 `check` 报告计数，并扫描每个平面的可读性：

```console
$ drsg --db graph.drsg snapshot backup.drsgsnap
$ drsg --db fresh.drsg  restore  backup.drsgsnap
$ drsg --db graph.drsg stats
$ drsg --db graph.drsg check
```

## 提供服务

`serve` 是嵌入式模型的例外：它打开数据库，随后将其通过网络暴露给仪表盘、各 SDK 与
MCP 服务。

```console
$ DRSG_TOKEN=please-change-me drsg --db graph.drsg serve --addr 0.0.0.0:7700
```

服务及其配置见[第 2 章](./getting-started.md#运行服务)，仪表盘见
[第 5 章](./web-ui.md)，客户端见[第 6 章](./sdk.md)。
