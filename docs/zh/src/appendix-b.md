# 附录 B：查询语言文法

openCypher 子集的完整文法，即解析器所接受的形式。仪表盘将其查询页签标注为
**GraphQL**；该语言并非 GraphQL，而是一个 openCypher 子集，本附录即其参考。
[第 4 章](./query-language.md)讲的是各构造**用来做什么**；本附录则精确说明**什么能被
解析**。

记号约定：`|` 表示择一，`[…]` 表示可选，`{…}` 表示零次或多次，`'…'` 表示一个字面
记号。关键字**不区分大小写**（`match` 等同于 `MATCH`）；标识符则区分。

## 语句

```text
statement      ::= read-query | write-statement
```

读语句编译为一个逻辑计划并返回行；写语句改动平面并返回变更计数。

## 读

```text
read-query     ::= source { beam } [ where ] return
                   [ order-by ] [ skip ] [ limit ] [ as-of ]
```

每一种数据源都恰好绑定一个节点模式，并可继续接一段关系尾巴，因此一次带类型的跳转
之于检索或算法种子，与之于一个 `MATCH` 节点完全一样。

```text
source         ::= 'MATCH'  pattern
                 | 'SEARCH' node-pat vector-seed  tail
                 | 'SEARCH' node-pat keyword-seed tail
                 | 'HYBRID' node-pat { hybrid-part } tail
                 | 'CALL'   ident '(' [ call-args ] ')' 'ON' node-pat tail

tail           ::= { rel-pat node-pat }

vector-seed    ::= [ 'ON' ident ] 'NEAR' vec-arg [ 'METRIC' metric ] [ 'TOPK' uint ]
keyword-seed   ::= 'ON' ident 'MATCHING' string [ 'TOPK' uint ]

hybrid-part    ::= 'VECTOR'  [ 'ON' ident ] 'NEAR' vec-arg
                     [ 'METRIC' metric ] [ 'WEIGHT' number ]
                 | 'KEYWORD' 'ON' ident 'MATCHING' string [ 'WEIGHT' number ]
                 | 'GRAPH'   'HOPS' uint [ 'DECAY' number ]
                     [ 'SEEDS' uint ] [ 'WEIGHT' number ]
                 | 'CANDIDATES' uint
                 | 'TOPK' uint

call-args      ::= call-arg { ',' call-arg }
call-arg       ::= ident ':' value
```

`HYBRID` 至少需要 `VECTOR` 与 `KEYWORD` 之一——`GRAPH` 只是对二者所找到的结果加以
提升。各部分书写次序任意，每种至多出现一次。

### 遍历

```text
beam           ::= 'BEAM' node-pat direction [ ':' ident ] [ 'ON' ident ]
                   'NEAR' vec-arg [ 'METRIC' metric ] 'WIDTH' uint 'DEPTH' uint

direction      ::= 'OUT' | 'IN' | 'BOTH'

pattern        ::= node-pat { rel-pat node-pat }
node-pat       ::= '(' [ ident ] [ ':' ident ] ')'
rel-pat        ::= [ '<' ] '-' [ '[' rel-body ']' ] '-' [ '>' ]
rel-body       ::= [ ident ] [ ':' ident ] [ var-len ]
var-len        ::= '*' [ uint ] [ '..' [ uint ] ]
```

关系的方向由其箭头决定：`-[…]->` 为出边，`<-[…]-` 为入边，`-[…]-` 为双向；`<-…->`
会被拒绝。关系变量可以书写，但不会被绑定。`var-len` 必须有上界（`*1..3`、`*2`、
`*..4`）；无界的 `*` 或 `*2..` 会明确报错。

### 尾部子句

```text
where          ::= 'WHERE' expr
return         ::= 'RETURN' [ 'DISTINCT' ] ( ident | '*' )
order-by       ::= 'ORDER' 'BY' order-key { ',' order-key }
order-key      ::= expr [ 'ASC' | 'DESC' ]
skip           ::= 'SKIP' uint
limit          ::= 'LIMIT' uint
as-of          ::= 'AS' 'OF' ( uint | string | 'TIME' int )
```

`RETURN` 只能指名模式中的**最后**一个变量，或 `*`。`AS OF` 位于最后：裸整数为提交
序号，带引号的字符串为一个 RFC-3339 时刻，`TIME` 后接 unix 纪元毫秒数。

## 表达式

由最松到最紧：

```text
expr           ::= or-expr
or-expr        ::= and-expr { 'OR' and-expr }
and-expr       ::= not-expr { 'AND' not-expr }
not-expr       ::= 'NOT' not-expr | comparison
comparison     ::= additive [ 'IS' [ 'NOT' ] 'NULL'
                            | 'IN' '[' [ expr { ',' expr } ] ']'
                            | cmp-op additive ]
cmp-op         ::= '=' | '<>' | '!=' | '<' | '<=' | '>' | '>='
additive       ::= multiplicative { ( '+' | '-' ) multiplicative }
multiplicative ::= unary { ( '*' | '/' ) unary }
unary          ::= '-' unary | primary
primary        ::= '(' expr ')' | param | literal | term
term           ::= ident '.' ident            (* 属性 *)
                 | ident ':' ident            (* 标签判定 *)
                 | function
function       ::= 'score' '(' ')'
                 | 'hops' '(' ')'
                 | 'key' '(' ident ')'
                 | ( 'similarity' | 'distance' )
                   '(' ident '.' ident ',' vec-arg [ ',' metric ] ')'
```

`score()` 是该行的 score 通道——种子的相关度，或算法的逐节点结果。`hops()` 为路径
长度。`key(n)` 为节点的外部键；作用于源变量时，`key(n) = "…"` 与 `key(n) IN […]`
会编译为一次键查找，而非先扫描再过滤。`x IN [a, b]` 是等值判断的语法糖。

一个 `WHERE` 条件只能引用一个模式变量——编译器会将其安放在该变量于流水线中的位置。

## 写

```text
write-statement ::= 'CREATE' create-path { ',' create-path }
                  | 'MERGE'  create-path { merge-on }
                  | 'MATCH'  pattern [ 'WHERE' expr ] mutate-op { mutate-op }

merge-on       ::= 'ON' ( 'CREATE' | 'MATCH' ) 'SET' set-item { ',' set-item }

mutate-op      ::= 'SET' set-item { ',' set-item }
                 | 'REMOVE' remove-item { ',' remove-item }
                 | [ 'DETACH' ] 'DELETE' ident { ',' ident }
                 | 'CREATE' create-path { ',' create-path }
                 | 'MERGE'  create-path { merge-on }

set-item       ::= ident '.' ident '=' value
                 | ident ':' ident
                 | ident '+=' prop-map
remove-item    ::= ident '.' ident | ident ':' ident

create-path    ::= create-node { create-rel create-node }
create-node    ::= '(' [ ident ] [ ':' ident ] [ prop-map ] ')'
create-rel     ::= [ '<' ] '-' '[' ':' ident [ prop-map ] ']' '-' [ '>' ]
prop-map       ::= '{' [ prop-entry { ',' prop-entry } ] '}'
prop-entry     ::= ident ':' value
```

在 `create-node` 中，取字符串值的 `key:` 条目设定节点的外部键，而不会成为一个属性。
新建的边必须有向，且必须指明类型。`MATCH … mutate-op` 作用于模式的末端变量。

## 终结符

```text
ident          ::= ( letter | '_' ) { letter | digit | '_' }
uint           ::= digit { digit }
int            ::= [ '-' ] uint
number         ::= [ '-' ] digit { digit } [ '.' digit { digit } ]
string         ::= '"' { 除 '"' 外的任意字符 } '"'
                 | "'" { 除 "'" 外的任意字符 } "'"
vector         ::= '[' [ number { ',' number } ] ']'
vec-arg        ::= string | vector
metric         ::= 'cosine' | 'dot' | 'l2'
param          ::= '$' ident
literal        ::= number | string | 'true' | 'false' | 'null' | vector
value          ::= param | literal
```

本轮中字符串没有转义序列，因此同类引号无法出现在字符串内部。记号之间的空白不具意义；
不支持注释。`$name` 参数出现在值的位置，并在解析时从调用方的参数映射中解析取值——这
是传值时免于注入的方式。

## 默认值

| 省略项 | 取值 | 缘由 |
|---|---|---|
| `NEAR` 之前的 `ON <属性>` | `embedding` | 摄取流水线所写入的属性 |
| `METRIC` | `cosine` | |
| `TOPK` | `10` | |
| `HYBRID … CANDIDATES` | `100` | 融合前每通道的候选池 |
| `GRAPH … DECAY` | `0.5` | 与 RPC、MCP 及命令行各接口一致 |
| `GRAPH … SEEDS` | `10` | 每通道取作种子的最高命中数 |
| `WEIGHT` | 向量 `1.0`、关键词 `1.0`、图 `0.5` | |
| `CALL pagerank` 参数 | `damping: 0.85, iterations: 20, tolerance: 1e-6` | |
| `CALL louvain` 参数 | `max_levels: 10, min_gain: 1e-9` | |
| `CALL shortest_path` 的 `dir` | `out` | `from` 与 `to` 为必需 |

`MATCHING` 没有属性默认值：关键词索引按 `(标签, 属性)` 声明，而这些属性并无约定俗成
的命名，故 `ON` 为必需——出于同样的理由，标签亦为必需。

## 算法

```text
CALL 'pagerank'      ( [ damping: number ] [ iterations: uint ] [ tolerance: number ] )
CALL 'components'    ( )                      (* 别名：connected_components *)
CALL 'louvain'       ( [ max_levels: uint ] [ min_gain: number ] )
CALL 'shortest_path' ( from: key-or-id , to: key-or-id [ , dir: string ]
                       [ , weight: string ] )
```

`key-or-id` 为一个字符串（外部键）或一个整数（节点 id）。每个算法都通过 `score()`
报告其逐节点结果：`pagerank` 为 rank，`components` 与 `louvain` 为从 0 起的紧凑分组
序号，`shortest_path` 为该节点在路径上的位次。未知的算法名或参数名会报错，而绝不会
被悄然忽略。

## 尚不支持

以下每一项都会明确报错，而绝不会被静默地错误编译：

- 投影与聚合——`RETURN a.name, count(*)`、`GROUP BY`、`WITH` 流水化。行模型只携带
  一个当前节点，故这些需要一套多绑定的行契约（这是刻意的推迟）。
- 返回或排序依据模式中最后一个变量之外的任何变量。
- 跨越两个变量的谓词——`WHERE p.year < q.year`。
- 复用模式变量，那将表达一个图约束。
- 分支模式——每条语句只允许一条线性路径。
- 无界的可变长度关系——`*`、`*2..`。
- 无向或未指明类型的新建边。
- 注释、字符串转义，以及 `WITH`。
