// Unit tests for the hand-written transport, driven through a `fetch` double
// so no server is involved: how a timeout, a refused connection and a
// non-2xx reply each surface. The e2e suite covers the happy paths.
import { expect, test } from "bun:test";
import { Client, DrsgError, DrsgTimeoutError, type FetchLike } from "../src/index";

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
