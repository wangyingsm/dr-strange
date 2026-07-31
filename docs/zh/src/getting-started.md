# 快速上手

本章介绍如何从源码构建 Dr Strange、初始化数据库、在命令行中发起查询，以及运行
服务——既包括本地方式，也包括以容器镜像方式部署。

## 环境准备

- 当前的 **Rust 工具链**（stable 通道），经 [rustup](https://rustup.rs) 安装。
- 用于 Web 仪表盘：**[bun](https://bun.sh)** 编译单页应用，以及可选的
  **[just](https://github.com/casey/just)** 作为任务运行器。
- 用于容器工作流：**Docker**（Engine 24+，启用 BuildKit）。

构建通过 rustls/ring 链接 TLS，无需 OpenSSL 工具链。

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

## 磁盘布局

`--db` 参数用于选择数据库路径。在原生后端下，数据库是一个**目录**——预写日志（WAL）
与有序的 SST 文件驻留其中——并伴有两个存放检索索引的伴生文件：

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
`OPENAI_API_KEY`）；针对字面量向量的查询则无需任何提供方。

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

**鉴权。** 未配置令牌时，仅同源的浏览器界面被授权调用接口。若要允许来自 SDK 或
`curl` 的程序化访问，请配置一个共享令牌，并以 Bearer 凭据形式携带：

```console
$ DRSG_TOKEN=please-change-me drsg --db graph.drsg serve
```

## 容器镜像

一个多阶段 `Dockerfile` 会编译仪表盘、构建内嵌该仪表盘的二进制，并产出一个精简的
运行时镜像。构建并运行：

```console
$ docker build -t dr-strange:latest .
$ docker run -p 7700:7700 -v drsg-data:/data \
    -e DRSG_TOKEN=please-change-me \
    dr-strange:latest
```

运行时镜像绑定到 `0.0.0.0:7700`，并将数据库存放于 `/data` 卷（原生后端数据库是一个
目录，由该卷持久化）。提供方密钥以环境变量形式提供。

对于持久化部署，`docker-compose.yml` 以具名卷定义了一个等价的服务：

```console
$ DRSG_TOKEN=please-change-me docker compose up --build
```

## 下一步

- **第 3 章 —— AI 原生：** 嵌入、混合检索、自然语言查询与文档导入。
- **第 4 章 —— 查询语言：** openCypher 子集及其底层的逻辑计划。
- 按访问方式：**第 6 章 —— SDK**（应用代码）、**第 7 章 —— 嵌入式命令行**（运维）、
  **第 8 章 —— MCP**（LLM 智能体）。
