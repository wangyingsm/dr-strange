# SDK

Dr Strange 提供**六种语言**的客户端库——TypeScript、Python、Go、Java、C 与 Zig。
每一种都通过 JSON-RPC 2.0 与正在运行的 `drsg serve` 通信，且其带类型的方法接口是
**由服务端的 OpenRPC schema 生成的**，因此每个 SDK 都与线协议精确一致，并在各版本间
与之保持同步。（Zig 客户端是对生成的 C 客户端的一层胶水封装，因此继承同样的保证。）

## 获取 SDK

各 SDK 位于仓库的 `sdk/<语言>` 目录下。在软件包尚未发布到各语言的仓库之前，可将相应
目录纳入（vendor）你的项目，或就地依赖它：

| 语言 | 位置 | 构建 / 引入 |
|---|---|---|
| TypeScript | `sdk/typescript` | 一个 `package.json` 模块（bun / npm） |
| Python | `sdk/python` | 一个 `pyproject.toml` 包（`pip install`） |
| Go | `sdk/go` | 模块 `github.com/wangyingsm/dr-strange/sdk/go` |
| Java | `sdk/java` | 一个 Maven 模块（Jackson + JDK HttpClient） |
| C | `sdk/c` | `make` → `libdrsg.a` + `drsg.h`（libcurl + json-c） |
| Zig | `sdk/zig` | 一个 `build.zig` 模块，胶水封装 C 客户端（Zig 0.16） |

## 连接与调用

客户端由一个基地址与一个令牌构造；令牌默认取自 `DRSG_TOKEN` 环境变量，并以
`Authorization: Bearer` 凭据的形式随每个请求一同发送。方法名与 RPC 方法一一对应，
并适配各语言的命名习惯：

| 语言 | 构造客户端 | 调用示例 |
|---|---|---|
| TypeScript | `new Drsg({ baseUrl, token })` | `await db.nodeCreate({ … })` |
| Python | `Drsg(base_url=…, token=…)` | `db.node_create(…)` |
| Go | `drsg.New(drsg.WithBaseURL(…), drsg.WithToken(…))` | `db.NodeCreate(ctx, …)` |
| Java | `new Drsg(baseUrl, token)` | `db.nodeCreate(…)` |
| C | `drsg_client_new(base_url, token)` | `drsg_node_create(…)` |
| Zig | `try drsg.Client.init(base_url, token)` | `c.drsg_node_create(client.handle, …)` |

其形态是统一的。以 TypeScript 为例：

```typescript
import { Drsg } from "drsg";

const db = new Drsg({ baseUrl: "http://127.0.0.1:7700", token: process.env.DRSG_TOKEN });

await db.nodeCreate({ plane: "social", key: "ada", labels: ["Person"] });
await db.nodeCreate({ plane: "social", key: "alan", labels: ["Person"] });
await db.edgeCreate({ plane: "social", src: "ada", dst: "alan", type: "KNOWS" });

const stats = await db.dbStats();
console.log(stats.nodes, stats.edges);
```

其它语言以各自的惯用法遵循同一套方法接口——Go 在每次调用中传入一个
`context.Context`，Python 与 Java 抛出异常，C 返回一个由调用方拥有的 `json_object`，
并通过一个出参报告失败。

## 错误处理

应用级失败（未知平面、非法计划）是一个 JSON-RPC 错误；被拒绝的凭据对应错误码
`-32001`。各 SDK 以带类型的错误呈现之：TypeScript 与 Python 中的 `DrsgError` /
`DrsgAuthError`，Go 中带 `IsAuthError` 的 `*drsg.Error`，Java 中的 `DrsgException` /
`DrsgAuthException`，以及 C 中一个填充好的 `drsg_error`（配 `drsg_is_auth_error`）。

## 变更流

每个 SDK 都能打开一条长连接 WebSocket，订阅某个平面的变更流（[第 3 章](./ai-native.md)），
接收每一个已提交的 `ChangeEvent`——`{ plane, seq, truncated, changes }`，其中每个变更
为 `{ kind, op, id, labels?, record? }`。订阅遵循各语言自然的并发模型：

**TypeScript** —— 一个回调；套接字自动重连。`close()` 停止它。

```typescript
const sub = db.watch("social", (event) => {
  for (const c of event.changes) console.log(event.seq, c.op, c.kind, c.id);
});
// sub.close();
```

**Python** —— 一个阻塞式生成器；迭代以消费，跳出以断开。

```python
for event in db.watch("social"):
    for c in event["changes"]:
        print(event["seq"], c["op"], c["kind"], c["id"])
```

**Go** —— 一个通道（channel）；取消 context 以停止。

```go
events, _ := db.Watch(ctx, "social")
for e := range events {
    for _, c := range e.Changes {
        fmt.Println(e.Seq, c.Op, c.Kind, c.ID)
    }
}
```

**Java** —— 一个监听器；返回的 `Subscription` 关闭它。

```java
var sub = db.watch("social", null, event -> {
    for (var c : event.changes()) System.out.println(event.seq() + " " + c.op() + " " + c.kind());
});
// sub.close();
```

**C** —— 一个回调；`drsg_watch` 阻塞，直到回调返回非零值（如有需要，请在一个线程上
运行它）。

```c
static int on_change(struct json_object *event, void *userdata) {
    /* 检视 event["changes"]；返回非零值以停止 */
    return 0;
}
drsg_error err;
drsg_watch(client, "social", NULL, on_change, NULL, &err);
```

一个可选的标签会将订阅收窄到对该标签节点的变更。投递是尽力而为的：落后过多的订阅者
会丢弃溢出部分，而不会拖住写入方。

由于每个事件都携带其落库时的提交序号，订阅者可以读取图在该序号处的 `as_of` 状态——
以及其前一个序号处的 `as_of` 状态——从而重建一次变更的确切前后（[第 4 章](./query-language.md)）。

## 代码生成

每个 SDK 带类型的方法接口都由 `crates/dr-strange-web/openrpc.json` 生成，这也是服务端
从 `rpc.discover` 返回的唯一权威来源。每个 SDK 都携带一个小型代码生成器与一个漂移
（drift）测试；一旦已提交的客户端不再与该 schema 匹配，测试便会失败，因此这些库无法
在无声无息中偏离线协议。手写的部分——传输层、错误类型，以及 WebSocket 的 `watch`——
位于生成的接口之下。
