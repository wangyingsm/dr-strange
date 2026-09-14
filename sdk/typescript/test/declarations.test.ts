// The emitted `.d.ts` must type-check for consumers that do not share the
// SDK's own ambient types. Three are modelled: a browser project (`lib: DOM`,
// no `@types/bun`), a current Node project (`@types/node`, no DOM), and a
// runtime whose typings know `fetch` but no `WebSocket` — Node 18/20's, the
// oldest the README supports. Each hands its platform `fetch` (and, where it
// has one, `WebSocket`) to the client's option types; a `typeof fetch` /
// `typeof WebSocket` in the declarations is exactly what used to bite there.
import { expect, test } from "bun:test";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const ROOT = resolve(import.meta.dir, "..");
const TSC = join(ROOT, "node_modules", "typescript", "bin", "tsc");

function tsc(args: string[]): { code: number; out: string } {
  const r = Bun.spawnSync(["bun", TSC, ...args], { cwd: ROOT, stdout: "pipe", stderr: "pipe" });
  return { code: r.exitCode, out: r.stdout.toString() + r.stderr.toString() };
}

const WITH_WEBSOCKET = `
import { Drsg, DrsgTimeoutError, type FetchLike, type WebSocketConstructor } from "../dist/index";
const f: FetchLike = fetch;
const ws: WebSocketConstructor = WebSocket;
const db = new Drsg({ fetch: f, timeoutMs: 5 });
db.watch("p", (e) => e.seq, { WebSocket: ws });
export const isTimeout = (e: unknown): boolean => e instanceof DrsgTimeoutError;
`;

const FETCH_ONLY = `
import { Drsg, type FetchLike } from "../dist/index";
const f: FetchLike = fetch;
export const db = new Drsg({ fetch: f, timeoutMs: 5 });
`;

// What a fetch-capable, WebSocket-less runtime's typings provide, in the
// shape @types/node 18/20 give them.
const FETCH_ONLY_GLOBALS = `
interface AbortSignal { readonly aborted: boolean; addEventListener(type: "abort", cb: () => void): void }
interface Response { readonly ok: boolean; readonly status: number; readonly statusText: string; json(): Promise<any> }
interface RequestInit { method?: string; headers?: Record<string, string>; body?: string; signal?: AbortSignal | null }
declare function fetch(input: string, init?: RequestInit): Promise<Response>;
`;

interface Consumer {
  options: object;
  app: string;
  globals?: string;
}

const CONSUMERS: Record<string, Consumer> = {
  browser: { options: { lib: ["ES2022", "DOM"], types: [] }, app: WITH_WEBSOCKET },
  node: { options: { lib: ["ES2022"], types: ["node"] }, app: WITH_WEBSOCKET },
  "fetch-only": {
    options: { lib: ["ES2022"], types: [] },
    app: FETCH_ONLY,
    globals: FETCH_ONLY_GLOBALS,
  },
};

test("the emitted declarations stand on their own for each consumer", () => {
  const dir = mkdtempSync(join(tmpdir(), "drsg-dts-"));
  try {
    const dist = join(dir, "dist");
    const emit = tsc([
      "-p", "tsconfig.json", "--declaration", "--emitDeclarationOnly", "--outDir", dist,
    ]);
    expect(emit.out).toBe("");
    expect(emit.code).toBe(0);

    for (const [name, consumer] of Object.entries(CONSUMERS)) {
      const app = join(dir, name);
      mkdirSync(app);
      writeFileSync(join(app, "app.ts"), consumer.app);
      const files = ["app.ts"];
      if (consumer.globals) {
        writeFileSync(join(app, "globals.d.ts"), consumer.globals);
        files.push("globals.d.ts");
      }
      writeFileSync(
        join(app, "tsconfig.json"),
        JSON.stringify({
          compilerOptions: {
            ...consumer.options,
            target: "ES2022",
            module: "ESNext",
            moduleResolution: "bundler",
            strict: true,
            noEmit: true,
            skipLibCheck: false,
            typeRoots: [join(ROOT, "node_modules", "@types")],
          },
          files,
        }),
      );
      const check = tsc(["-p", join(app, "tsconfig.json")]);
      expect(check.out, `${name} consumer`).toBe("");
      expect(check.code, `${name} consumer`).toBe(0);
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}, 120_000);
