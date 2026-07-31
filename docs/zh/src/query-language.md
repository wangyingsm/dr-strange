# 查询语言

Dr Strange 通过一个可序列化的**逻辑计划**来回答查询——它是一条由算子组成的、明确
的流水线（扫描、按键定位、扩展、过滤、排序、限量、向量 top-k）。你可以直接构造这个
计划，也可以用一套 **openCypher 子集**语言来书写，它会编译成同一个计划。

## Cypher 子集

```text
MATCH (p:Person)-[:KNOWS]->(q:Person)
WHERE p.age >= 18
RETURN q
LIMIT 50
```

读操作（`MATCH … RETURN`）返回一张子图；写操作（`CREATE`、`MERGE`、`SET`、
`REMOVE`、`DELETE`）执行变更并报告变更计数。值可以作为 `$name` 参数传入，而不必
拼接进查询文本。

## 查询中的相似度检索

```text
SEARCH (d:Doc) ON embedding NEAR "some text" TOPK 10 RETURN d
```

`NEAR "文本"` 会在服务端把文本嵌入；`NEAR $vec` 则接收一个字面量向量。

## 图算法

在某个平面的单一快照上进行的、只读且瞬态的运行（可作用于整个平面，或限定到某个
标签）：PageRank、弱连通分量、带权最短路径，以及 Louvain 社区发现。

## 时间旅行

任何读取都可以用一个 **AS OF** 地址（一个提交序号或一个时间戳）钉在某个历史时刻，
从而看到图当时的确切样子。

## 小节（草拟）

- 逻辑计划：各算子，以及一条查询如何变成一个计划
- Cypher 子集：`MATCH` / `WHERE` / `RETURN` / `ORDER BY` / `SKIP` / `LIMIT`
- 写操作：`CREATE` / `MERGE` / `SET` / `REMOVE` / `DELETE`，以及 `$params`
- `SEARCH … NEAR … TOPK`：文本 vs. 字面量向量的相似度
- 表达式、谓词与属性访问
- 图算法（`plane.algo`）及其选项
- 时间旅行（`AS OF <序号 | 时间戳>`）及其保证
- 直接运行计划 vs. 通过语言书写（二者等价）
