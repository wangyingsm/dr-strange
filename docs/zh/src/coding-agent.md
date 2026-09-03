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
OpenCode、Gemini CLI 或 Codex CLI 各自写入一份匹配的 MCP 配置——但只有这个工具
自己的标记（它会创建的目录，或者已经拥有的配置文件）已经出现在这个仓库里，才会
写入。

```console
$ drsg init
plane 'myrepo' bootstrapped — serve watch pid 48213, http://127.0.0.1:51900/mcp
  + wrote ./.mcp.json
  + Cursor: wrote ./.cursor/mcp.json
```

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

所以：每个仓库运行一次，此后只要服务没了就再运行一次。

## 事实为智能体带来什么

依赖 grep 工作的智能体，每个问题都要重建一遍结构：搜索、打开文件、阅读、推断谁
调用了什么，再来一遍。已图化的平面把这份工作提前做完了，只做一次，由解析器完成。
于是结构性问题就变成了**一次往返**，而不是一轮搜索加阅读的循环。

八个动词承担主要工作，在 MCP（[第 8 章](./mcp.md)）与 CLI（[第 7 章](./embedded-cli.md)）
上完全一致——不过 `grep` 与 `snippet` 需要读取被监视的源码树，因此只随服务端
提供：

| 动词 | 它回答的问题 |
|---|---|
| `context` | 关于一个符号的一切——定义、带调用位置的调用者、被调用者、引用——首选动词 |
| `search` | “我不知道名字”：在平面的向量嵌入上做语义 top-k |
| `describe` | 一个符号的属性——只看节点的轻量视图 |
| `grep` | 在被监视的源码树上做字面文本检索，有界且带计数 |
| `trace` | 一个符号如何到达另一个：图中记录的最短调用路径 |
| `impact` | 影响范围：所有能到达该符号的东西，按距离分组 |
| `fathom` | 一个符号身处怎样的地方：几跳之内的区域，按标签与边类型计数，并给出枢纽 |
| `snippet` | 一个符号的源码文本 |

每个回答都是紧凑的、每行一条事实的文本，尺寸是按模型的上下文窗口定的，而不是按
终端。`context` 会收紧各分组的条数上限，把自己保持在固定预算之内，并写明省略了
什么。

在 `serve watch` 之下，图会跟踪每一次提交。变更过的文件重新经过插件，平面随之
就地更新：新符号被创建，消失的符号被删除，边被改写，最终收敛到和一次全量重新
图化完全相同的结果。每个回答都以 `synced: commit <sha>` 开头，智能体因此知道
自己推理的是*哪一版*代码。工作区里未提交的修改，在真正提交之前都不可见，而回答
会通过注明提交号把这一点说清楚。

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
