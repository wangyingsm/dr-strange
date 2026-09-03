# 附录 B：查询语言文法

这是 openCypher 子集的完整文法，也就是解析器实际接受的形式。仪表盘把查询页签标注为
**GraphQL**，但这门语言并不是 GraphQL，而是一个 openCypher 子集，本附录正是它的参考。
[第 4 章](./query-language.md)讲的是各构造**用来做什么**，本附录讲的则是**什么能被
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

每一种数据源都恰好绑定一个节点模式，之后可以继续接一段关系尾巴：无论种子来自检索、
算法，还是普通的 `MATCH` 节点，带类型跳转的写法都完全一样。

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

`HYBRID` 至少需要 `VECTOR` 或 `KEYWORD` 之一，`GRAPH` 只是在二者找到的结果上做
提升。各部分的书写顺序任意，但每种至多出现一次。

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
return         ::= 'RETURN' [ 'DISTINCT' ] return-item { ',' return-item }
return-item    ::= ident | '*'                (* 行本身 *)
                 | ( expr | aggregate ) [ 'AS' ident ]
aggregate      ::= 'count' '(' '*' ')'
                 | agg-func '(' [ 'DISTINCT' ] expr ')'
agg-func       ::= 'count' | 'sum' | 'avg' | 'min' | 'max' | 'collect'
order-by       ::= 'ORDER' 'BY' order-key { ',' order-key }
order-key      ::= ( expr | ident ) [ 'ASC' | 'DESC' ]
skip           ::= 'SKIP' uint
limit          ::= 'LIMIT' uint
as-of          ::= 'AS' 'OF' ( uint | string | 'TIME' int )
```

`RETURN n` 或 `RETURN *` 交回完整记录，且只能指名模式中的**最后**一个变量。其余任
何一项都是**投影**：它成为一列，列名取 `AS` 别名，未写别名时取该项的原文；它可以读
取模式所绑定的任一变量。节点不能与列同处一个 `RETURN`——节点不是值。

`aggregate` 在每个组上折叠；分组键是所有不是聚合的列。折叠会跳过 null，也跳过它无
法采用的值；当一组什么也没提供时，`count` 与 `sum` 为 `0`，`avg`/`min`/`max` 为
`null`，`collect` 为空列表。

在投影查询中，`DISTINCT` 比较整个投影行，`ORDER BY` 指名所返回的某一列（用别名，或
用它所返回的那个表达式），`SKIP`/`LIMIT` 计的是投影行。而在 `RETURN n` 之下，这四者
仍取各自的节点语义。`AS OF` 位于最后：裸整数为提交序号，带引号的字符串为一个
RFC-3339 时刻，`TIME` 后接 unix 纪元毫秒数。

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

`score()` 是该行的 score 通道，取值是种子的相关度，或算法给出的逐节点结果。
`hops()` 是路径长度。`key(n)` 是节点的外部键；用在源变量上时，`key(n) = "…"` 和
`key(n) IN […]` 会编译成一次键查找，而不是先扫描再过滤。`x IN [a, b]` 只是等值
判断的语法糖。

一个 `WHERE` 条件只能引用一个模式变量，编译器会把它安放在这个变量在流水线中的
位置上。

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

目前字符串不支持转义序列，因此同类引号不能出现在字符串内部。记号之间的空白没有
意义，也不支持注释。`$name` 参数写在值的位置上，解析时会从调用方的参数映射里取值，
这样传值就不必担心注入问题。

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

`MATCHING` 没有默认属性：关键词索引按 `(标签, 属性)` 声明，这些属性没有约定俗成的
命名，所以 `ON` 是必需的；出于同样的理由，标签也是必需的。

## 算法

```text
CALL 'pagerank'      ( [ damping: number ] [ iterations: uint ] [ tolerance: number ] )
CALL 'components'    ( )                      (* 别名：connected_components *)
CALL 'louvain'       ( [ max_levels: uint ] [ min_gain: number ] )
CALL 'shortest_path' ( from: key-or-id , to: key-or-id [ , dir: string ]
                       [ , weight: string ] )
```

`key-or-id` 可以是一个字符串（外部键），也可以是一个整数（节点 id）。每个算法都
通过 `score()` 报告逐节点结果：`pagerank` 给出 rank，`components` 和 `louvain` 给出
从 0 开始的紧凑分组序号，`shortest_path` 给出该节点在路径上的位次。未知的算法名或
参数名一律报错，绝不会被悄悄忽略。

## 尚不支持

以下每一项都会明确报错，而绝不会被静默地错误编译：

- `WITH` 流水化——投影是尾部，其后不再接任何子句。
- 返回模式中最后一个变量之外的任何变量的**行**（一次跳跃之后的 `RETURN p`）；它的
  值仍可投影（`RETURN p.name`）。
- 非投影查询按模式中最后一个变量之外的变量排序。
- 跨越两个变量的谓词——`WHERE p.year < q.year`。
- 复用模式变量，那将表达一个图约束。
- 分支模式——每条语句只允许一条线性路径。
- 无界的可变长度关系——`*`、`*2..`。
- 无向或未指明类型的新建边。
- 注释、字符串转义，以及 `WITH`。
