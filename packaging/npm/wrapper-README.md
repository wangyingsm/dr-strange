# @yingge/drsg

A code-intelligence graph for coding agents. Digests a repository into a
resolved call graph and answers *who calls this*, *what would a change reach*,
*how does one symbol reach another* and *which HTTP routes meet* — one round
trip each, with no model in the loop.

```console
$ npx @yingge/drsg init   # digest, serve, and write the MCP config
```

Your agent then has `context`, `impact`, `trace`, `wire`, `grep` and the rest.
On this project — 97k lines, 13 parser plugins — the digest takes about three
seconds, and answering "who calls this and what would a change reach" costs a
median 925 tokens against 53,720 for grepping the name and reading the files
that matched.

The installed command is `drsg`. Installing the package pulls exactly one
prebuilt binary, chosen by your platform:
`drsg-linux-x64`, `drsg-linux-arm64`, `drsg-darwin-x64`, `drsg-darwin-arm64`
or `drsg-win32-x64`.

Full documentation, the measurement method, and the standalone installer:
**https://github.com/wangyingsm/dr-strange**

MIT OR Apache-2.0.
