# 嵌入式命令行

`drsg` 是面向本地数据库的命令行工具。它直接打开数据库文件（嵌入式——无需服务），
因此非常适合脚本化、导入、检视、备份与运维。每条命令都接受一个全局的
`--db <路径>`。

## 一趟巡览

```console
$ drsg --db graph.drsg plane list
$ drsg --db graph.drsg import nodes.jsonl --plane social
$ drsg --db graph.drsg cypher --plane social 'MATCH (n:Person) RETURN n'
$ drsg --db graph.drsg algo pagerank --plane social --top 10
$ drsg --db graph.drsg ask 'who does Ada know?' --plane social
$ drsg --db graph.drsg snapshot backup.drsgsnap
$ drsg --db graph.drsg serve
```

## 备份与恢复

`drsg snapshot <out>` 会在单一提交序号上写出一个一致的整库快照包；
`drsg restore <in>` 则把它重建进一个全新（空）的数据库，并保留 id、提交序号，以及
已构建好的检索索引。

## 小节（草拟）

- 全局选项（`--db`、配置文件）与输出约定
- 平面生命周期（`plane create/list/show/drop`）
- 数据进出（`import` / `export` JSONL、`get`）
- 查询（`query` 一个计划、`cypher`、`catalog`）
- 图算法（`algo pagerank/components/shortest-path/louvain`）
- 检索（`hybrid`、`index`、`ask`）
- 备份（`snapshot` / `restore`）与完整性（`check`、`stats`）
- 提供服务（`serve`）及其与 SDK 的关系
