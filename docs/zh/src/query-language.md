# 查询语言

Dr Strange 中的每一次读与写，都作为一个**逻辑计划**执行——一条由算子组成的、明确
的流水线。计划既可以作为一个可序列化的结构直接书写，也可以用一套 **openCypher
子集**语言书写，后者会编译为同一个计划。二者等价；该语言只是计划之上的一层表层
表示。

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
（`n:Label`）以及布尔运算符 `AND`、`OR`、`NOT`。读操作返回所匹配的子图——每条匹配
路径上的节点与边，而不仅是末端节点。

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

值可以作为 `$name` 参数提供，而非拼接进查询文本——这既保持查询稳定，又免去转义：

```text
MATCH (p:Person) WHERE p.age >= $min RETURN p
```

## 查询中的相似度检索

`SEARCH` 子句将相似度作为行的一个数据源，于是一次语义查找与一次遍历得以在同一条
语句中组合：

```text
SEARCH (d:Doc) ON embedding NEAR "how does time-travel work" TOPK 10 RETURN d
```

`ON <属性>` 选定向量属性；`TOPK <k>` 限定结果数量。文本参数（`NEAR "…"`）在服务端
被嵌入；字面量向量（`NEAR $vec`）则无需任何提供方。此子句编译为一个 `VectorTopK`
数据源，可从其继续进行 `MATCH` 式的遍历。

## 图算法

图算法独立于查询语言：它们是在某个平面的单一快照之上进行的、只读且瞬态的计算，
按名调用并返回一个结果集，而非改动图。每个算法作用于整个平面，或——在指定标签
时——作用于该标签的诱导子图。

```console
$ drsg --db graph.drsg algo pagerank      --plane social --top 10
$ drsg --db graph.drsg algo components    --plane social
$ drsg --db graph.drsg algo shortest-path --plane social --src 1 --dst 42
$ drsg --db graph.drsg algo louvain       --plane social
```

- **PageRank** —— 重要性得分，最重要者在前（可控制阻尼、迭代与收敛容差）。
- **连通分量** —— 每个节点所属的弱连通分量（以该分量中最小的 id 代表）。
- **最短路径** —— 两个节点之间的一条带权最短路径，沿选定方向，可选地以某个数值型
  边属性为权重。
- **Louvain** —— 通过模块度优化得到的社区划分。

## 时间旅行

任何读取都可以被钉在某个历史时刻——一个提交序号或一个时间戳——从而看到图当时的
确切样子。这是一个**读取选项**，而非语言子句，且其本质是只读的：它选定查询所读取的
快照，无法改动历史。

时间旅行以 RPC 接口与各 SDK 读取方法上的 `as_of`（提交序号）或 `as_of_ms`（时间戳）
参数、嵌入式 API 中的 `PlaneHandle::as_of(…)`，以及仪表盘中的 **Time-travel** 滑块
与历史搜索来表达。两种形式均采用"在此刻或之前"的语义：介于两次提交之间的取值，解析
为不晚于它的最近一次提交。

可查询的窗口由 `plane.history` 以最旧/最新提交序号对的形式报告。历史默认无界保留；
可以配置一个保留窗口对其加以限制，此时早于该窗口的读取将被拒绝。

由于每个变更事件都携带其落库时的提交序号，智能体可以读取 `as_of(seq)` 与
`as_of(seq - 1)`，从而重建任意一次已提交变更的确切前后状态（见[第 6 章](./sdk.md)）。
