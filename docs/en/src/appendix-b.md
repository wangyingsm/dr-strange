# Appendix B: Query-Language Grammar

The complete grammar of the openCypher subset, as the parser accepts it. The
dashboard labels its query tab **GraphQL**; the language is not GraphQL but an
openCypher subset, and this appendix is its reference. [Chapter
4](./query-language.md) explains what each construct is *for*; this one states
exactly what parses.

Notation: `|` alternatives, `[…]` optional, `{…}` zero or more, `'…'` a literal
token. Keywords are **case-insensitive** (`match` = `MATCH`); identifiers are
not.

## Statement

```text
statement      ::= read-query | write-statement
```

A read compiles to a logical plan and returns rows; a write mutates the plane
and returns change counts.

## Reads

```text
read-query     ::= source { beam } [ where ] return
                   [ order-by ] [ skip ] [ limit ] [ as-of ]
```

Every source binds exactly one node pattern and may continue with a
relationship tail, so a typed hop follows a retrieval or algorithm seed exactly
as it follows a `MATCH` node.

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

`HYBRID` requires at least one of `VECTOR` or `KEYWORD` — `GRAPH` only boosts
what those find. Parts may appear in any order, each at most once.

### Traversal

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

A relationship's direction comes from its arrows: `-[…]->` out, `<-[…]-` in,
`-[…]-` both. `<-…->` is rejected. A relationship variable parses but is not
bound. `var-len` must have an upper bound (`*1..3`, `*2`, `*..4`); an unbounded
`*` or `*2..` is a clear error.

### Tail clauses

```text
where          ::= 'WHERE' expr
return         ::= 'RETURN' [ 'DISTINCT' ] ( ident | '*' )
order-by       ::= 'ORDER' 'BY' order-key { ',' order-key }
order-key      ::= expr [ 'ASC' | 'DESC' ]
skip           ::= 'SKIP' uint
limit          ::= 'LIMIT' uint
as-of          ::= 'AS' 'OF' ( uint | string | 'TIME' int )
```

`RETURN` names the pattern's **last** variable, or `*`. `AS OF` is last: a bare
integer is a commit sequence, a quoted string an RFC-3339 instant, and `TIME`
takes unix-epoch milliseconds.

## Expressions

Loosest to tightest:

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
term           ::= ident '.' ident            (* a property *)
                 | ident ':' ident            (* a label test *)
                 | function
function       ::= 'score' '(' ')'
                 | 'hops' '(' ')'
                 | 'key' '(' ident ')'
                 | ( 'similarity' | 'distance' )
                   '(' ident '.' ident ',' vec-arg [ ',' metric ] ')'
```

`score()` is the row's score channel — a seed's relevance, or an algorithm's
per-node result. `hops()` is the path length. `key(n)` is the node's external
key; on the source variable, `key(n) = "…"` and `key(n) IN […]` compile to a
key seek rather than a scan-and-filter. `x IN [a, b]` is sugar for equalities.

A `WHERE` condition may reference only one pattern variable — the compiler
places it at that variable's position in the pipeline.

## Writes

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

In a `create-node`, a string-valued `key:` entry sets the node's external key
rather than becoming a property. A created edge must be directed and must name
a type. `MATCH … mutate-op` operates on the pattern's terminal variable.

## Terminals

```text
ident          ::= ( letter | '_' ) { letter | digit | '_' }
uint           ::= digit { digit }
int            ::= [ '-' ] uint
number         ::= [ '-' ] digit { digit } [ '.' digit { digit } ]
string         ::= '"' { any character except '"' } '"'
                 | "'" { any character except "'" } "'"
vector         ::= '[' [ number { ',' number } ] ']'
vec-arg        ::= string | vector
metric         ::= 'cosine' | 'dot' | 'l2'
param          ::= '$' ident
literal        ::= number | string | 'true' | 'false' | 'null' | vector
value          ::= param | literal
```

Strings have no escape sequences in this cut, so a quote cannot appear inside a
string of the same kind. Whitespace between tokens is insignificant; there are
no comments. A `$name` parameter stands where a value does, resolved from the
caller's parameter map at parse time — the injection-safe way to pass values.

## Defaults

| Omitted | Value | Why |
|---|---|---|
| `ON <property>` before `NEAR` | `embedding` | what the ingestion pipeline writes |
| `METRIC` | `cosine` | |
| `TOPK` | `10` | |
| `HYBRID … CANDIDATES` | `100` | per-channel pool before fusion |
| `GRAPH … DECAY` | `0.5` | matches the RPC, MCP and CLI surfaces |
| `GRAPH … SEEDS` | `10` | top hits per channel used as seeds |
| `WEIGHT` | vector `1.0`, keyword `1.0`, graph `0.5` | |
| `CALL pagerank` args | `damping: 0.85, iterations: 20, tolerance: 1e-6` | |
| `CALL louvain` args | `max_levels: 10, min_gain: 1e-9` | |
| `CALL shortest_path` `dir` | `out` | `from` and `to` are required |

`MATCHING` has no property default: keyword indexes are declared per `(label,
property)` and those properties follow no convention, so `ON` is required — as
is a label, for the same reason.

## Algorithms

```text
CALL 'pagerank'      ( [ damping: number ] [ iterations: uint ] [ tolerance: number ] )
CALL 'components'    ( )                      (* alias: connected_components *)
CALL 'louvain'       ( [ max_levels: uint ] [ min_gain: number ] )
CALL 'shortest_path' ( from: key-or-id , to: key-or-id [ , dir: string ]
                       [ , weight: string ] )
```

`key-or-id` is a string (an external key) or a whole number (a node id). Each
algorithm reports its per-node result through `score()`: the rank for
`pagerank`, a dense 0-based group index for `components` and `louvain`, and the
node's position along the path for `shortest_path`. An unknown algorithm or
argument name is an error, never a silently ignored setting.

## Not supported

Each of these is a clear error, never a silent mis-compile:

- projections and aggregation — `RETURN a.name, count(*)`, `GROUP BY`, `WITH`
  pipelining. The row model carries one current node, so these need a
  multi-binding row contract (a deliberate deferral).
- returning or ordering by any variable but the pattern's last.
- predicates spanning two variables — `WHERE p.year < q.year`.
- reusing a pattern variable, which would express a graph constraint.
- branching patterns — one linear path per statement.
- unbounded variable-length relationships — `*`, `*2..`.
- undirected or untyped created edges.
- comments, string escapes, and `WITH`.
