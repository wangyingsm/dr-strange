# 快速上手

本章带你从一个空目录，走到一张能在命令行里查询、在浏览器里浏览的图。

## 环境准备

- 较新的 **Rust 工具链**（stable），可通过 [rustup](https://rustup.rs) 安装。
- 可选（用于 Web 仪表盘）：**[bun](https://bun.sh)**（构建单页应用）与
  **[just](https://github.com/casey/just)**（任务运行器）。

## 构建

Dr Strange 是一个 Cargo 工作区。构建命令行工具 `drsg`：

```console
$ cargo build --release -p dr-strange-cli
```

这会生成 `target/release/drsg`。默认构建使用原生 LSM 存储引擎；如果确有需要，还可以
通过特性开关启用一个遗留的 redb 后端。

若想把**真正的仪表盘**（而非占位页）打包进二进制，请先构建 Web 单页应用——它会在
编译期被嵌入：

```console
$ just web-build          # bun install + vite build
$ cargo build --release -p dr-strange-cli
```

## 磁盘上的数据库

用 `--db` 指定一个路径。在原生后端下，数据库是一个**目录**（预写日志 WAL 与有序的
SST 文件都在其中），旁边另有两个用于检索索引的伴生文件：

```text
graph.drsg/          ← 数据库（WAL + SST 文件）
graph.drsg.hnsw      ← 向量索引伴生文件
graph.drsg.bm25      ← 关键词索引伴生文件
```

数据库在首次使用时自动创建，无需单独的"初始化"步骤。

## 你的第一张图

先创建一个平面，再写入一些数据。你可以用 openCypher 子集来写：

```console
$ drsg --db graph.drsg plane create social

$ drsg --db graph.drsg cypher --plane social \
    'CREATE (a:Person {name:"Ada"}),
            (b:Person {name:"Alan"}),
            (a)-[:KNOWS]->(b)'
```

读回来看看：

```console
$ drsg --db graph.drsg cypher --plane social \
    'MATCH (p:Person)-[:KNOWS]->(q:Person) RETURN q'
```

查看数据的整体形态：

```console
$ drsg --db graph.drsg stats
$ drsg --db graph.drsg catalog --plane social
```

## 加入嵌入并按语义检索

向量就是普通属性。先在某个 `(标签, 属性)` 对上声明索引，再检索它。对*文本*查询做
嵌入是在服务端完成的，因此进程的环境中需要一个提供方密钥（例如 `OPENAI_API_KEY`）；
或者，你也可以直接用一个字面量向量检索，无需任何提供方。

```console
$ drsg --db graph.drsg index ensure Doc embedding --plane social

$ OPENAI_API_KEY=… drsg --db graph.drsg cypher --plane social \
    'SEARCH (d:Doc) ON embedding NEAR "a friendly greeting" TOPK 5 RETURN d'
```

由于结果本身就是图节点，你可以从它们继续向外遍历——这正是第 1 章讲的 GraphRAG 模式。

## 启动仪表盘

```console
$ drsg --db graph.drsg serve
```

这会启动 JSON-RPC 接口、WebSocket 变更流以及内嵌的仪表盘，并打印出地址
（默认 `http://127.0.0.1:7700`）。打开它，即可浏览图、导入文档、运行查询、实时观察
变更。

**鉴权。** 未设置令牌时，只有同源的浏览器界面可以调用接口。若要允许程序化访问
（SDK、`curl`），请在启动服务前设置一个共享令牌，并以 Bearer 令牌的形式携带：

```console
$ DRSG_TOKEN=please-change-me drsg --db graph.drsg serve
```

## 下一步去哪

- **第 3 章 —— AI 原生：** 嵌入、混合检索、自然语言查询与文档导入。
- **第 4 章 —— 查询语言：** 完整的 openCypher 子集，以及其下的逻辑计划。
- 更喜欢写代码？直接看 **第 6 章 —— SDK**。更喜欢用命令行？看 **第 7 章 ——
  嵌入式命令行**。在构建智能体？看 **第 8 章 —— MCP**。
