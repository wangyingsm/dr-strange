# 查询语言

Dr Strange 中的每一次读与写，都作为一个**逻辑计划**执行：一条由算子组成、语义
明确的流水线。计划既可以直接写成可序列化的结构，也可以用一套 **openCypher
子集**语言书写，后者会编译成同一个计划。二者完全等价，这套语言只是计划之上的
一层表层表示。

本章讲解各构造的用途；完整文法见[附录 B](./appendix-b.md)：每个子句、每项默认
值，以及每一项刻意不予支持的内容。

## 逻辑计划

一个计划由一个**数据源（source）**，后接一串**步骤（step）**构成。每一行沿流水线
流动，携带其当前节点，以及产生它的那条路径。

数据源：

| 数据源 | 产生的行 |
|---|---|
| `ScanAll` | 平面内的每个节点 |
| `ScanLabel(标签)` | 携带该标签的节点 |
| `SeekKeys(键)` | 由外部键解析出的节点 |
| `VectorTopK{…}` | 与查询向量最近的 *k* 个节点 |
| `KeywordTopK{…}` | 与文本查询最相关的 *k* 个节点（BM25） |
| `Hybrid{…}` | 按向量 + 关键词 + 图邻近融合排序的 *k* 个节点 |
| `Algo{…}` | 某个图算法产出的节点，各自携带其结果 |

每一种数据源产出的行都会继续流经后续步骤，因此检索与算法并非终结操作：一个关键词
命中、一个融合命中或一个 PageRank 结果，都能像任何扫描得到的节点一样，继续参与
遍历、过滤、排序与限量。

步骤：

| 步骤 | 作用 |
|---|---|
| `Expand{dir, edge_type}` | 遍历一跳（方向，边类型可选） |
| `ExpandVar{…}` | 遍历可变数量的跳数 |
| `Filter(expr)` | 保留谓词成立的行 |
| `Distinct` | 按当前节点去重 |
| `Sort(keys)` | 对行排序 |
| `Skip(n)` / `Limit(n)` | 偏移 / 限量结果 |

计划是可序列化的。`MATCH (:Person)-[:KNOWS]->(q) RETURN q LIMIT 50` 对应于：

```json
{
  "source": { "ScanLabel": "Person" },
  "steps": [
    { "Expand": { "dir": "Out", "edge_type": "KNOWS" } },
    { "Limit": 50 }
  ]
}
```

以此形式表示的计划可直接运行：

```console
$ drsg --db graph.drsg query - --plane social < plan.json
```

## Cypher 子集

读操作以 `MATCH … RETURN` 表达，并可由 `WHERE`、`ORDER BY`、`SKIP`、`LIMIT` 与
`DISTINCT` 进一步细化：

```text
MATCH (p:Person)-[:KNOWS]->(q:Person)
WHERE p.age >= 18
RETURN q
ORDER BY q.name
LIMIT 50
```

一个 `MATCH` 是一个节点模式，可选地通过关系模式（`-[:TYPE]->`、`<-[:TYPE]-`，或
无向）串接。`WHERE` 谓词组合了属性比较（`=`、`<>`、`<`、`<=`、`>`、`>=`）、标签测试
（`n:Label`）、字符串谓词（`CONTAINS`、`STARTS WITH`、`ENDS WITH`）、成员判断
（`IN`）以及布尔运算符 `AND`、`OR`、`NOT`。读操作返回所匹配的子图——每条匹配路径上
的节点与边，而不仅是末端节点。

### 文本谓词与成员判断

```text
MATCH (d:Doc) WHERE d.title CONTAINS "graph"    RETURN d
MATCH (p:Person) WHERE p.name STARTS WITH "Al"  RETURN p
MATCH (f:File) WHERE f.path ENDS WITH ".pdf"    RETURN f
```

匹配按字节进行：不做大小写折叠，与 `=` 的立场一致。非字符串标量会提升为其文本形式，
因此无论 `year` 存为 `2026` 还是 `"2026"`，`d.year STARTS WITH "20"` 都成立。这一点
对软模式数据很重要，因为同一字段在不同节点上的类型可能并不一致。没有规范文本形式的值
（`Null`、字节串、向量、列表、映射）则一律不匹配。

`IN` 表示成员判断，并且刻意不写作 `CONTAINS`：对于一个字符串列表，「contains」既可
理解为「含有该元素」，也可理解为「某个元素含有该子串」，而语法本身无法在二者之间做出
选择。

```text
MATCH (d:Doc) WHERE "graph" IN d.tags  RETURN d   -- 列表属性中的元素
MATCH (d:Doc) WHERE "author" IN d.meta RETURN d   -- 映射属性中的键
MATCH (n) WHERE n.year IN [2020, 2021] RETURN n   -- 字面量列表
```

列表按元素判断，所用相等语义与 `=` 相同，因此 `7` 可匹配存储的 `7.0`。映射按**键**
判断，而非按值。右侧为字面量列表时会在编译期展开为若干等值判断；其余形式则逐行求值。

> 谓词不匹配与属性缺失这两种情形无法区分，因此对于一份根本没有 `title` 的文档，
> `NOT (d.title CONTAINS "x")` 同样成立。若需要区分，请使用 `d.title IS NULL`。

### 锚定到某个已知实体

`key(n)` 读取节点的外部键，即创建该节点时所用的稳定标识。它是一个普通的项，凡可
写表达式之处皆可使用；但若它在查询的**首个**变量上构成等值（或 `IN`）判断，就会
编译为一个 `SeekKeys` 数据源：这是一次索引查找，而非先扫描再过滤。

```text
MATCH (n:Doc) WHERE key(n) = "paper-42" RETURN n
MATCH (n) WHERE key(n) IN ["ada", "alan"] RETURN n
```

有些图的身份信息存于键中而不是某个属性里，LLM 摄取生成的素材常常就是这一类；对
这种图来说，这正是关键写法。它也能从某个特定实体出发，锚定一次遍历：

```text
MATCH (n)-[:CITES]->(p:Paper)
WHERE key(n) = "paper-42"
RETURN p
```

## 写操作

写子句改动平面，并报告变更计数而非返回行：

| 子句 | 作用 |
|---|---|
| `CREATE` | 创建节点与边 |
| `MERGE` | 匹配一个既有模式，或将其创建 |
| `SET` | 新增或覆盖属性或标签 |
| `REMOVE` | 移除属性或标签 |
| `DELETE` | 删除一个节点或边 |
| `DETACH DELETE` | 删除一个节点及其相连的边 |

```text
CREATE (a:Person {name:"Ada"})
MERGE (b:Person {name:"Alan"})
CREATE (a)-[:KNOWS {since: 1936}]->(b)
```

值可以通过 `$name` 参数提供，而不必拼接进查询文本，这样既能保持查询稳定，又省去
了转义：

```text
MATCH (p:Person) WHERE p.age >= $min RETURN p
```

## 查询中的相似度检索

`SEARCH` 子句将相似度作为行的一个数据源，于是一次语义查找与一次遍历得以在同一条
语句中组合：

```text
SEARCH (d:Doc) ON embedding NEAR "how does time-travel work" TOPK 10 RETURN d
```

`ON <属性>` 用于选定向量属性，也可以省略：`NEAR` 默认取 `embedding`，也就是文档
摄取流水线写入的那个属性，因此 `SEARCH (d:Doc) NEAR "…"` 就是它的简写形式。
`TOPK <k>`
限定结果数量。文本参数（`NEAR "…"`）在服务端被嵌入；字面量向量（`NEAR $vec`）则无需
任何提供方。此子句编译为一个 `VectorTopK` 数据源，可从其继续进行遍历：

```text
SEARCH (d:Doc) ON embedding NEAR "time travel" TOPK 10
-[:CITES]->(p:Paper)
WHERE p.year >= 2020
RETURN p
```

## 查询中的关键词检索

把 `NEAR` 换成 `MATCHING`，同一个动词便检索词语而非语义。它编译为一个
`KeywordTopK` 数据源，作用于在该 `(标签, 属性)` 对上声明的 BM25 索引，且每一行都以
`score()` 携带其相关度：

```text
SEARCH (d:Doc) ON body MATCHING "graph database" TOPK 10
RETURN d ORDER BY score() DESC
```

此处标签与 `ON <属性>` 均为必需。关键词索引按 `(标签, 属性)` 声明，二者缺一便无从
检索；且与向量属性不同，关键词属性并无一个值得作为默认的约定名。与可回退为精确扫描
的向量检索不同，未声明索引时关键词检索返回空结果。

## 查询中的混合检索

`HYBRID` 将至多三路排序通道融合为单一次序。各通道均为可选（`VECTOR` 与 `KEYWORD`
至少需其一），可携带 `WEIGHT`，且书写次序任意。每个通道只有其定义性的部分是必需
的：`GRAPH` 需要 `HOPS`，另两者需要一个查询。因此 `VECTOR NEAR "…"` 与
`GRAPH HOPS 2` 就是简写形式：向量属性默认取 `embedding`，逐跳衰减默认取 `0.5`，
这与 RPC、MCP 和命令行的默认值保持一致：

```text
HYBRID (d:Doc)
  VECTOR ON embedding NEAR "graph database internals" METRIC cosine WEIGHT 1.0
  KEYWORD ON body MATCHING "LSM storage engine" WEIGHT 1.0
  GRAPH HOPS 2 DECAY 0.5 WEIGHT 0.5
  CANDIDATES 100 TOPK 10
RETURN d ORDER BY score() DESC
```

这正是[第 3 章](./ai-native.md)中 `plane.hybrid` 检索在语言中的表达，并经由同一套
融合引擎运行：每个通道在加权求和前先做 min-max 归一化，`score()` 则携带融合后的
结果。

## 图算法

图算法是在某个平面的单一快照之上进行的、只读且瞬态的计算。它们可作为查询的数据源
使用，因而其输出可继续供给流水线的其余部分：

```text
CALL pagerank(damping: 0.85, iterations: 20) ON (n:Paper)
RETURN n ORDER BY score() DESC LIMIT 10

CALL shortest_path(from: "ada", to: "alan", dir: "both") ON (n)
RETURN n

CALL components() ON (n:Doc) RETURN n
CALL louvain(max_levels: 10) ON (n:Doc) RETURN n
```

`ON (v[:标签])` 身兼两职：它将算法限定在该标签的诱导子图上（省略标签则为整个平面），
并绑定查询其余部分所引用的变量。所有参数皆为可选，缺省即取引擎自身的默认值；未知的
算法名或参数名会报错，而不会被悄然忽略。

由于行模型只携带一个当前节点，各算法均通过 score 通道报告其逐节点结果：

| 算法 | 行的次序 | `score()` |
|---|---|---|
| `pagerank` | 最重要者在前 | 该节点的 rank |
| `components` | 按分量分组 | 从 0 起的紧凑分量序号 |
| `louvain` | 按社区分组 | 从 0 起的紧凑社区序号 |
| `shortest_path` | 源点 → 终点 | 该节点在路径上的位次 |

`shortest_path` 的两个端点以外部键（字符串）或节点 id（整数）给出，另可指定
`out`/`in`/`both` 之一的 `dir`，以及指向某个数值型边属性的 `weight`。端点未知或终点
不可达时返回空结果，而非报错。

由于算法本身即数据源，其结果可继续组合：

```text
CALL pagerank() ON (n:Paper)
-[:CITES]->(q:Paper)
WHERE q.year >= 2020
RETURN q ORDER BY score() DESC LIMIT 10
```

同样这些算法也可以直接以命令调用，此时报告的是原始结果而非行流：

```console
$ drsg --db graph.drsg algo pagerank      --plane social --top 10
$ drsg --db graph.drsg algo components    --plane social
$ drsg --db graph.drsg algo shortest-path --plane social --src 1 --dst 42
$ drsg --db graph.drsg algo louvain       --plane social
```

## 时间旅行

任何读取都可以锁定在某个历史时刻——一个提交序号或一个时间戳，看到图在当时的确切
样子。这类读取本质上是只读的：它只是选定查询所读取的快照，不会改动历史。

在语言中，这就是 `AS OF` 子句，写在最后，读起来就是整条查询之上的一个修饰。它
接受一个提交序号、一个 RFC-3339 时刻，或 `TIME` 后接 unix 纪元毫秒数，并且对每
一种数据源都适用：扫描、检索种子或算法皆可：

```text
MATCH (p:Paper)-[:CITES]->(q:Paper) RETURN q LIMIT 10 AS OF 41337

SEARCH (d:Doc) ON body MATCHING "outage" TOPK 5
RETURN d AS OF "2026-07-01T00:00:00Z"

CALL pagerank() ON (n:Paper) RETURN n AS OF TIME 1782864000000
```

同一个历史地址在语言之外亦可获得：作为 RPC 接口与各 SDK 读取方法上的 `as_of`
（提交序号）或 `as_of_ms`（时间戳）参数、嵌入式 API 中的 `PlaneHandle::as_of(…)`，
以及仪表盘中的 **Time-travel** 滑块与历史搜索。所有形式均采用“在此刻或之前”的语义：
介于两次提交之间的取值，解析为不晚于它的最近一次提交。

时间旅行的读取无法使用向量索引，因为该索引只按最新提交构建；这类相似度检索转而
扫描被钉住的快照，结果依然正确，只是不经索引加速。时间旅行需要原生后端。

可查询的窗口由 `plane.history` 以最旧/最新提交序号对的形式报告。历史默认无界保留；
可以配置一个保留窗口对其加以限制，此时早于该窗口的读取将被拒绝。

由于每个变更事件都携带其落库时的提交序号，智能体可以读取 `as_of(seq)` 与
`as_of(seq - 1)`，从而重建任意一次已提交变更的确切前后状态（见[第 6 章](./sdk.md)）。
