# 插件

`drsg digest` 遍历代码仓库时，不会让模型去猜测代码的结构，而是把每个源码文件分发给
一个**预处理插件**去处理。插件是一个沙箱化的 WebAssembly 组件，内部跑的是编译器级
的解析器，返回的是**事实**：解析器确信无疑的节点与边。AST 不需要推断 `parse()`
调用了 `lex()`，它本来就知道。

插件位于独立的仓库
[dr-strange-extension](https://github.com/wangyingsm/dr-strange-extension)。
与数据库分开是有意为之：官方身份并不意味着步调必须锁死，解析器修一个缺陷不必等
数据库发版，数据库发版也不必等八条工具链都跟上。这个仓库就是扩展的公共地：官方
插件、契约的规范副本，以及编写自己插件所需的 SDK（后者是[编码智能体](./coding-agent.md)
一章后半部分的主题）。

## 一次 digest 如何使用插件

路由器按扩展名分配文件：每个已安装的插件声明自己处理的扩展名集合（`.rs`、`.py`、
`.html`……），没有插件认领的文件回落到内置的文档读取器，作为散文交给模型。被认领
的文件如何处理由插件决定，分两个阶段：

1. **`parse`**——宿主把分派的文件切成固定大小的块，对它们**并行**运行 `parse`，
   每次调用一个全新的沙箱实例，彼此不共享任何状态。每次调用返回一个*部分结果*
   （partial）：宿主只负责搬运、从不读取的一段不透明字节。
2. **`assemble`**——只调用**一次**，携带按块序排列的全部部分结果。跨文件解析
   （导入、头文件、桶式再导出、接口满足）就发生在这里，在插件内部，因为这是语言
   语义，而数据库拒绝持有任何语言语义。结果不得依赖块边界落在何处。

输入是**拉取而非推送**。插件拿到的是文件*路径*，需要什么就通过宿主读取，代码
解析器正是这样跟随一条导入进入相邻文件的。没有跨文件结构的格式可以把 `assemble`
当作拼接；SDK 恰好提供了这样的默认实现。

## 契约

插件与宿主之间的契约是一个小小的 [WIT](https://component-model.bytecodealliance.org/design/wit.html)
world：`drsg:preprocess@1.0.0`，规范副本位于扩展仓库根目录，drsg 侧存有受校验的
副本（任何副本漂移都会让 `just check-wit` 失败）：

```wit
interface host {
  %list: func(suffix: string) -> result<list<string>, string>;
  read:  func(path: string) -> result<list<u8>, string>;
  label: func() -> option<string>;
}

interface preprocessor {
  describe: func() -> manifest;                          // name, version, extensions
  parse:    func(subject: input, options: list<tuple<string, string>>)
              -> result<list<u8>, string>;               // one chunk → an opaque partial
  assemble: func(partials: list<list<u8>>, options: list<tuple<string, string>>)
              -> result<output, string>;                 // all partials, in order → facts
}
```

这三个 `host` 函数**就是**能力授予的全部：插件能触及的只有写在这里的东西。
`%list` 返回被消化根目录下可读的路径，且**已排序**：未排序的目录顺序会让两次运行
的输出不同，而重新导入同一棵树本应得到同一张图。`read` 返回单个文件的字节，任何
解析后越出根目录的路径都会被拒绝（检查落在解析后的路径上，`..` 与符号链接都绕不
过去）。`label` 在输入内容无法自述名字时提供一个名字。

插件的 `output` 包含节点、边、散文，以及一份**报告**：事实数、散文字符数、跳过
的输入数，外加一段说明什么没能解析、为什么没能解析的文字注记。计数并点名，而不是悄悄丢弃：
一张偏薄的图应当由它的报告来解释，而不是靠换参数重跑导入来排查。`options` 携带
操作者配置中 `[plugins.<name>]` 一节的插件自有设置，原样透传。

## 插件不得调用 LLM——这是规则

插件永远不调用语言模型。官方插件不调用，第三方插件同样不调用：这条规则适用于每
一个插件，而且**由沙箱强制执行，而非仅仅要求作者自觉**。沙箱没有网络
（`wasi:sockets` 在加载时按名拒绝）、没有环境变量，除三个宿主函数外也没有文件
系统，因此插件根本无从触及任何模型服务，也拿不到调用所需的密钥。

分工正是意义所在。插件的职责是解析器能*证明*的东西，且是确定性的；真正需要模型的
内容（一段文档注释的含义、一份 README、任何解析器无法断言为事实的东西）则作为
**散文**返回，由*宿主*的 digest 流水线决定是否交给模型，在操作者的密钥、预算与
`--mode` 之下进行。只产出事实的代码仓库，导入全程**不发起一次模型调用**。

这条界线在图里同样保持可见：

- 解析得到的事实携带 `_generated_by`（如 `rust@2`）而非 `_model`，因此永远能与
  模型的抽取区分开。当两者声称同一个键时，**事实获胜**，模型的主张被丢弃并计数。
- 确定性是契约的一部分，不是一个愿望。沙箱冻结时钟、按固定序列发放熵、对目录列表
  排序，因此同一棵树消化两次，得到逐字节相同的事实。插件内部的模型调用恰恰会破坏
  这一点。

## 沙箱

每个插件都是运行在全面拒绝授权之下的 `wasm32-wasip2` 组件。插件的语言运行时可以
*导入* `wasi:filesystem`（Go 的运行时在插件第一行代码执行之前就会这么做），但其
背后的预打开表是空的，什么也读不到；`wasi:sockets` 在加载时按名拒绝；时钟被冻结；
熵是固定的；每次调用都在指令与内存预算之下运行。

插件在沙箱里崩溃时，它写到 stderr 的内容、以及陷入的原因（trap code）本身，都会
一并收进报给操作者的错误信息，方便定位问题。插件的全部产出都以**返回值**的形式交回
宿主，写数据库的永远只有宿主。

一次调用只解析**一个文件**，所以插件在某个文件上栽了跟头，损失也就是这一个文件：
它会被跳过并计数，报告里会点名。最常见的元凶是生成代码——`.pb.go` 里那个由上千段
字符串拼接而成的描述符，会让递归下降的打印器一路走出插件链接时定下的栈；这是插件
作者要修的，任何宿主配置都抬不高那个栈。但如果插件在它认领的**每一个**文件上都失败，
那就是另一回事了，这一趟仍然会被判定为失败。

预算可在 `drsg.toml` 中调整（[第 2 章](./getting-started.md#配置文件)）：

```toml
[plugins]
fuel = 200000000000    # 每次沙箱调用的指令预算（0 为不设限）
memory_mb = 3072       # 每次调用的线性内存上限，MiB；按照 wasm32 标准，最高支持 4096

[plugins.rust]         # 插件自有设置，原样透传
include_source = true
```

拉取模型还带来一条边界：预处理在文件所在之处运行。CLI 与 stdio MCP 服务会经过
预处理；经网络送达共享 `drsg serve` 的字节保持为散文。唯一的刻意例外是
`serve watch`：操作者把服务端指向其自己机器上的一个仓库（这是一次明确的文件系统
授权），于是提交折叠会经过已安装的插件。

## 官方目录

八个官方插件覆盖常用语言，每个都包装一个成熟的解析器而非重新发明：

| 插件 | 处理的扩展名 | 底层解析器 |
|---|---|---|
| `rust` | `.rs` | [syn](https://crates.io/crates/syn) |
| `go` | `.go` | Go 自带的 `go/parser`，经 TinyGo 编译 |
| `ts` | `.ts .tsx .mts .cts .js .jsx .mjs .cjs` | [swc](https://swc.rs)——同时支持 ESM 与 CommonJS |
| `py` | `.py .pyi .pyw` | [ruff](https://github.com/astral-sh/ruff) 的解析器 |
| `java` | `.java` | [tree-sitter-java](https://github.com/tree-sitter/tree-sitter-java) |
| `c` | `.c .h` | [tree-sitter-c](https://github.com/tree-sitter/tree-sitter-c) |
| `web` | `.html .htm .css` | tree-sitter html/css/js——一个插件同时处理，`class="btn"` 才能绑定到定义 `.btn` 的样式表 |
| `toml` | `.toml` | [toml](https://crates.io/crates/toml) |

每个插件以 `<plugin>-vX.Y.Z` 标签按自己的节奏发布；CI 构建组件并在
[发布页](https://github.com/wangyingsm/dr-strange-extension/releases)发布
`<plugin>.wasm` 与其 SHA-256。

这些 Release 的清单——`catalog.json`——与插件一同放在 extensions 仓库，而不在这个
二进制里。这正是要点：编译进 drsg 的目录会让每次插件发布都变成一次 drsg 发布，而
drsg 只是在重复一个既有事实。因此 `drsg plugin install` 去抓取它，一个插件名就足
以完成安装：

```console
$ drsg plugin install rust          # 目录中的一个名字
$ drsg plugin install              # 或交互式列表，0 = 全部
$ drsg plugin list --available     # 目录本身，并标注本地安装状态
```

抓取这份清单并不意味着盲目相信它。每个条目固定制品的 SHA-256，在下载后、把字节
当作组件看待之前就先校验；每个条目还说明自己面向哪种宿主——`contract` 是它构建时
依据的 WIT world，`min_drsg` 是它声称可用的最低宿主版本。本构建无法满足的条目会
**连同原因一起列出，而不是被隐藏**：一个从列表里悄悄消失的插件是一个待答的支持
问题，而「需要 drsg >= 3.0.0」本身就是答案。多个条目可以共用一个名字，这正是一个
插件继续服务旧宿主的方式——每个宿主安装它能运行的最新条目。

每次成功抓取都会缓存在已安装插件旁边，因此离线状态下的 `drsg plugin install` 仍
会列出清单并说明它有多旧。既无缓存又无网络时，它会报错并给出该 URL——因为不带目录
也一样能装插件：一个路径或一个 URL 不需要任何清单。

安装时会在插件库中固定制品的 SHA-256，之后每次加载都重新校验，磁盘上被改动的文件
会被拒绝而不是被悄悄运行。对同名插件再次安装即是升级路径。

## 每个官方解析器的承诺

八个解析器是一个家族，遵循同一条纪律：

- **键使用语言自己的全限定名**：`crate::module::fn`、`pkg.Type.Method`、
  `file.c::func`、`index.html#map`，绝不发明 id。
- **一切都携带位置**：定义带 `file` 与 `line`，边带它书写所在的行号。
- **已解析的边说明自己如何被解析**：每条边携带 `_resolved_by`（命中了哪条规则）、
  `_confidence`（一个档位，不是小数）与 `_ref`（调用处的原文）。
- **无法确知的东西被计数，绝不猜测。** 源码没有声明接收者类型的调用，会成为带
  `_reason` 的 `UnresolvedRef` 台账条目（可在图中查询、在 `context` 中呈现），
  而不是一条貌似合理的边。错误的边把智能体引向错误的地方；诚实声明的缺失，把它
  引向 `grep`。
- **树外之物用替身表示**，以书写原文作键、标记 `External`：记录"这段代码使用了
  那个东西"，而不假装读过从未见过的代码。

这些承诺正是[编码智能体](./coding-agent.md)一章的立足点，也是社区插件应当保持
的，那一章的后半部分会逐步演示。
