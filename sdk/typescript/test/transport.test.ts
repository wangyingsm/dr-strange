// Unit tests for the hand-written transport, driven through a `fetch` double
// so no server is involved: how a timeout, a refused connection and a
// non-2xx reply each surface. The e2e suite covers the happy paths.
import { expect, test } from "bun:test";
import { Client, DrsgError, DrsgTimeoutError, type FetchLike } from "../src/index";
import { wsAcceptsHeaders } from "../src/client";

const abortError = (): Error => {
  const e = new Error("The operation was aborted.");
  e.name = "AbortError";
  return e;
};

test("a request that outlives timeoutMs is a DrsgTimeoutError", async () => {
  // Never resolves on its own; only the client's abort ends it — the way a
  // stalled server looks to `fetch`.
  const hanging: FetchLike = (_url, init) =>
    new Promise((_resolve, reject) => {
      init.signal.addEventListener("abort", () => reject(abortError()));
    });
  const c = new Client({ token: "t", timeoutMs: 20, fetch: hanging });
  const err = await c["_call"]("db.stats").catch((e: unknown) => e);
  expect(err).toBeInstanceOf(DrsgTimeoutError);
  expect(err).toBeInstanceOf(DrsgError);
  const t = err as DrsgTimeoutError;
  expect(t.code).toBe(-32000);
  expect(t.timeoutMs).toBe(20);
  expect(t.message).toContain("timed out after 20 ms");
});

test("a refused connection stays a plain DrsgError", async () => {
  const refused: FetchLike = () => Promise.reject(new Error("ECONNREFUSED"));
  const c = new Client({ token: "t", timeoutMs: 1000, fetch: refused });
  const err = await c["_call"]("db.stats").catch((e: unknown) => e);
  expect(err).toBeInstanceOf(DrsgError);
  expect(err).not.toBeInstanceOf(DrsgTimeoutError);
  expect((err as DrsgError).message).toContain("connection failed: ECONNREFUSED");
});

test("an AbortError that is not ours is not reported as a timeout", async () => {
  // A caller-side abort (or a runtime quirk) can also raise AbortError; only
  // the client's own timer counts as a timeout.
  const aborted: FetchLike = () => Promise.reject(abortError());
  const c = new Client({ token: "t", timeoutMs: 1000, fetch: aborted });
  const err = await c["_call"]("db.stats").catch((e: unknown) => e);
  expect(err).toBeInstanceOf(DrsgError);
  expect(err).not.toBeInstanceOf(DrsgTimeoutError);
});

test("a well-formed reply returns its result", async () => {
  const ok: FetchLike = (_url, init) => {
    expect(init.method).toBe("POST");
    expect(init.headers.authorization).toBe("Bearer t");
    return Promise.resolve({
      ok: true,
      status: 200,
      statusText: "OK",
      json: () => Promise.resolve({ jsonrpc: "2.0", id: 1, result: { nodes: 3 } }),
    });
  };
  const c = new Client({ token: "t", fetch: ok });
  expect(await c["_call"]("db.stats")).toEqual({ nodes: 3 });
});

// The change feed's credential is a bearer header on the upgrade, never in
// the URL, wherever the WebSocket can carry one: a query-string token lands in
// proxy and access logs, and the server prefers the header (arch/08-web-ui
// §4.1). Only Bun's and Node's constructors take a `headers` init; the
// standard one (browser window or worker, Deno) does not, so `tokenInQuery`
// defaults to `!wsAcceptsHeaders()` and keeps `?token=` for those runtimes.
class RecordingSocket {
  static calls: Array<{ url: string; init?: { headers?: Record<string, string> } }> = [];
  onopen: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  constructor(url: string, init?: { headers?: Record<string, string> }) {
    RecordingSocket.calls.push({ url, init });
  }
  send(): void {}
  close(): void {}
}

test("watch sends the token as a bearer header and a bare /ws URL", () => {
  RecordingSocket.calls = [];
  const c = new Client({ baseUrl: "http://127.0.0.1:1", token: "s3cret" });
  const sub = c.watch("p", () => {}, { WebSocket: RecordingSocket, reconnect: false });
  sub.close();
  expect(RecordingSocket.calls.length).toBe(1);
  expect(RecordingSocket.calls[0].url).toBe("ws://127.0.0.1:1/ws");
  expect(RecordingSocket.calls[0].init?.headers).toEqual({ authorization: "Bearer s3cret" });
});

test("watch keeps ?token= only when asked for the browser form", () => {
  RecordingSocket.calls = [];
  const c = new Client({ baseUrl: "http://127.0.0.1:1", token: "a&b" });
  const sub = c.watch("p", () => {}, {
    WebSocket: RecordingSocket,
    reconnect: false,
    tokenInQuery: true,
  });
  sub.close();
  expect(RecordingSocket.calls[0].url).toBe("ws://127.0.0.1:1/ws?token=a%26b");
  expect(RecordingSocket.calls[0].init).toBeUndefined();
});

// Detection is by capability, not by the absence of `document`: a Web Worker
// has no document and no header-taking constructor, and Deno's node-compat
// layer exposes `process.versions.node` without accepting headers.
test("wsAcceptsHeaders says yes only for Bun and Node", () => {
  expect(wsAcceptsHeaders({})).toBe(false); // browser worker
  expect(wsAcceptsHeaders({ document: {} })).toBe(false); // browser window
  expect(wsAcceptsHeaders({ Deno: {} })).toBe(false);
  expect(wsAcceptsHeaders({ Deno: {}, process: { versions: { node: "20.0.0" } } })).toBe(false);
  expect(wsAcceptsHeaders({ Bun: {} })).toBe(true);
  expect(wsAcceptsHeaders({ process: { versions: { node: "22.0.0" } } })).toBe(true);
  expect(wsAcceptsHeaders()).toBe(true); // this suite runs under Bun
});

test("watch falls back to ?token= where the WebSocket takes no headers", () => {
  RecordingSocket.calls = [];
  const g = globalThis as { Deno?: unknown };
  g.Deno = {};
  try {
    const c = new Client({ baseUrl: "http://127.0.0.1:1", token: "s3cret" });
    const sub = c.watch("p", () => {}, { WebSocket: RecordingSocket, reconnect: false });
    sub.close();
  } finally {
    delete g.Deno;
  }
  expect(RecordingSocket.calls[0].url).toBe("ws://127.0.0.1:1/ws?token=s3cret");
  expect(RecordingSocket.calls[0].init).toBeUndefined();
});
