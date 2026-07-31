# SDK

Dr Strange 提供**五种语言**的客户端 SDK —— TypeScript、Python、Go、Java 与 C。
每一种都通过 JSON-RPC 2.0 与正在运行的 `drsg serve` 通信，且带类型的方法接口是
**由服务端的 OpenRPC schema 生成的**，因此每个 SDK 都始终与线协议精确一致。

## 处处一致的形态

用一个基地址和一个令牌（默认取自 `$DRSG_TOKEN`）构造客户端，随后调用与服务端
一一对应的方法：

```typescript
import { Drsg } from "drsg";

const db = new Drsg({ baseUrl: "http://127.0.0.1:7700", token: "…" });
await db.nodeCreate({ plane: "social", key: "ada", labels: ["Person"] });
console.log(await db.dbStats());
```

## 实时变更流

每个 SDK 都能打开一条长连接 WebSocket，订阅某个平面的变更流，在每一次变更提交时
即时收到它——TypeScript 通过回调（自动重连），Python 作为阻塞式生成器，Go 作为
通道（channel），Java 通过监听器，C 通过回调。

## 小节（草拟）

- 各 SDK 的安装 / 引入
- 连接：基地址、令牌与鉴权模型
- 读与写（生成的方法接口）
- 错误处理（`DrsgError` / 鉴权错误 `-32001`）
- 变更流：各语言中的 `watch`
- 代码生成：SDK 如何与 OpenRPC 保持同步
- 各语言的注意事项与惯用法
