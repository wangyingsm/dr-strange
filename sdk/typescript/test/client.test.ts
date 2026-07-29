// End-to-end tests: drive a real `drsg serve` over the client.
//
// Requires the `drsg` binary. Point at it with $DRSG_BIN, else the workspace
// `target/{debug,release}/drsg` is used; the suite skips if none is found.
import { afterAll, beforeAll, expect, test } from "bun:test";
import { type ChildProcess, spawn } from "node:child_process";
import { existsSync, mkdtempSync } from "node:fs";
import { connect, createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { DrsgAuthError, Drsg } from "../src/index";

const TOKEN = "test-token";
const ROOT = resolve(import.meta.dir, "..", "..", "..");

function findBinary(): string | null {
  const env = process.env.DRSG_BIN;
  if (env) return existsSync(env) ? env : null;
  for (const profile of ["debug", "release"]) {
    const cand = join(ROOT, "target", profile, "drsg");
    if (existsSync(cand)) return cand;
  }
  return null;
}

function freePort(): Promise<number> {
  return new Promise((res, rej) => {
    const srv = createServer();
    srv.once("error", rej);
    srv.listen(0, "127.0.0.1", () => {
      const addr = srv.address();
      const port = typeof addr === "object" && addr ? addr.port : 0;
      srv.close(() => res(port));
    });
  });
}

function canConnect(port: number): Promise<boolean> {
  return new Promise((res) => {
    const s = connect(port, "127.0.0.1");
    s.once("connect", () => {
      s.destroy();
      res(true);
    });
    s.once("error", () => res(false));
  });
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

let proc: ChildProcess | undefined;
let baseUrl = "";
const binary = findBinary();

beforeAll(async () => {
  if (!binary) return; // tests below early-return; see `skipIfNoBinary`
  const port = await freePort();
  const tmp = mkdtempSync(join(tmpdir(), "drsg-ts-"));
  const db = join(tmp, "sdk-test.drsg");
  proc = spawn(binary, ["--db", db, "serve", "--addr", `127.0.0.1:${port}`], {
    env: { ...process.env, DRSG_TOKEN: TOKEN },
    stdio: "ignore",
  });
  for (let i = 0; i < 100; i++) {
    if (await canConnect(port)) break;
    await sleep(50);
  }
  baseUrl = `http://127.0.0.1:${port}`;
});

afterAll(() => {
  proc?.kill();
});

// `test.skipIf` evaluates its guard at collection time — before beforeAll — so
// gate on the binary probe, which is resolved synchronously above.
const t = binary ? test : test.skip;

t("CRUD roundtrip", async () => {
  const db = new Drsg({ baseUrl, token: TOKEN });
  expect((await db.dbStats()).nodes).toBe(0);

  const alice = await db.nodeCreate({ plane: "startup", key: "alice", labels: ["Person"] });
  expect(alice.external_key).toBe("alice");
  await db.nodeCreate({ plane: "startup", key: "bob", labels: ["Person"] });

  const edge = await db.edgeCreate({ plane: "startup", src: "alice", dst: "bob", type: "KNOWS" });
  expect(edge.type).toBe("KNOWS");

  // Property patch: set then unset, with types preserved.
  let updated = await db.nodeUpdate({ plane: "startup", key: "alice", set: { age: 41, city: "NYC" } });
  expect(updated.properties.age).toBe(41);
  updated = await db.nodeUpdate({ plane: "startup", key: "alice", unset: ["city"] });
  expect(updated.properties.city).toBeUndefined();

  const got = await db.nodeGet({ plane: "startup", key: "alice" });
  expect(got?.properties.age).toBe(41);

  // Delete cascades the edge; the graph is left consistent.
  expect((await db.nodeDelete({ plane: "startup", key: "alice" })).deleted).toBe(true);
  const stats = await db.dbStats();
  expect([stats.nodes, stats.edges]).toEqual([1, 0]);
});

t("plane admin", async () => {
  const db = new Drsg({ baseUrl, token: TOKEN });
  expect((await db.planeCreate({ name: "notes" })).name).toBe("notes");
  expect((await db.planeRename({ plane: "notes", to: "archive" })).name).toBe("archive");
  expect((await db.planeDelete({ plane: "archive" })).deleted).toBe(true);
});

t("rpc.discover returns the OpenRPC document", async () => {
  const db = new Drsg({ baseUrl, token: TOKEN });
  const doc = await db.rpcDiscover();
  expect(doc.openrpc).toBe("1.2.6");
  const methods = doc.methods as Array<{ name: string }>;
  expect(methods.some((m) => m.name === "node.create")).toBe(true);
});

t("a bad token raises DrsgAuthError", async () => {
  const db = new Drsg({ baseUrl, token: "wrong" });
  await expect(db.dbStats()).rejects.toThrow(DrsgAuthError);
  try {
    await db.dbStats();
  } catch (e) {
    expect((e as DrsgAuthError).code).toBe(-32001);
  }
});
