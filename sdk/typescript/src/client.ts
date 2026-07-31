// Base JSON-RPC 2.0 transport for dr-strange (`drsg serve`).
//
// Zero runtime dependencies — just the platform `fetch`. The typed method
// surface lives in the generated `generated.ts` (see `codegen.mjs`); this
// module is the hand-written core it builds on.

export const DEFAULT_BASE_URL = "http://127.0.0.1:7700";

/** A JSON-RPC error returned by the server (carries the numeric `code`). */
export class DrsgError extends Error {
  readonly code: number;
  readonly data: unknown;

  constructor(code: number, message: string, data: unknown = undefined) {
    super(`${message} (code ${code})`);
    this.name = "DrsgError";
    this.code = code;
    this.data = data;
  }
}

/**
 * The server rejected the credential (code `-32001`).
 *
 * Set a valid token: `new Drsg({ token: … })` or the `DRSG_TOKEN` environment
 * variable. With no token configured server-side, only the same-origin browser
 * UI is authorized — a programmatic client must present one.
 */
export class DrsgAuthError extends DrsgError {
  constructor(code: number, message: string, data: unknown = undefined) {
    super(code, message, data);
    this.name = "DrsgAuthError";
  }
}

export interface DrsgOptions {
  /** Endpoint base, default `http://127.0.0.1:7700`. */
  baseUrl?: string;
  /** Shared bearer token; defaults to `$DRSG_TOKEN`. */
  token?: string;
  /** Per-request timeout in milliseconds, default 30000. */
  timeoutMs?: number;
  /** Override the `fetch` implementation (e.g. for tests). */
  fetch?: typeof fetch;
}

/** One node or edge that changed in a commit (the change feed — ROADMAP §5). */
export interface Change {
  kind: "node" | "edge";
  op: "created" | "updated" | "deleted";
  id: number;
  /** Node labels (absent for edges, and for a deleted node). */
  labels?: string[];
  /** The committed record for a create/update (embeddings + `_`-props stripped);
   *  absent for a delete — read `as_of(seq - 1)` for the before-state. */
  record?: Record<string, unknown>;
}

/** All the changes one commit produced, delivered over the change-feed socket. */
export interface ChangeEvent {
  plane: string;
  /** The commit sequence these changes landed at (address a time-travel read). */
  seq: number;
  /** The commit's change list was capped; some changes are omitted. */
  truncated: boolean;
  changes: Change[];
}

export interface WatchOptions {
  /** Only stream node changes carrying this label (edges pass unfiltered). */
  label?: string;
  /** Called on connect (`true`) and disconnect (`false`). */
  onState?: (open: boolean) => void;
  /** Auto-reconnect after a drop (default `true`); best-effort, re-sends the
   *  subscription. The feed itself is best-effort, so a reconnect can miss the
   *  commits that landed while disconnected. */
  reconnect?: boolean;
  /** WebSocket implementation, when there is no global one (Node < 21). */
  WebSocket?: typeof WebSocket;
}

/** A live change-feed subscription; call `close()` to stop it. */
export interface Subscription {
  close(): void;
}

/** One endpoint's worth of config plus the JSON-RPC call primitive. */
export class Client {
  readonly baseUrl: string;
  readonly token?: string;
  readonly timeoutMs: number;
  private readonly _fetch: typeof fetch;
  private _id = 0;

  constructor(opts: DrsgOptions = {}) {
    this.baseUrl = (opts.baseUrl ?? DEFAULT_BASE_URL).replace(/\/+$/, "");
    const envToken =
      typeof process !== "undefined" ? process.env?.DRSG_TOKEN : undefined;
    this.token = opts.token ?? envToken;
    this.timeoutMs = opts.timeoutMs ?? 30_000;
    const f = opts.fetch ?? globalThis.fetch;
    if (typeof f !== "function") {
      throw new Error("no global `fetch`; pass one via `fetch` in DrsgOptions");
    }
    this._fetch = f;
  }

  /** Send one JSON-RPC request and return its `result` (or throw). */
  protected async _call(method: string, params?: unknown): Promise<unknown> {
    this._id += 1;
    const payload: Record<string, unknown> = {
      jsonrpc: "2.0",
      method,
      id: this._id,
    };
    if (params !== undefined) payload.params = params;

    const headers: Record<string, string> = {
      "content-type": "application/json",
    };
    if (this.token) headers.authorization = `Bearer ${this.token}`;

    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), this.timeoutMs);
    let resp: Response;
    try {
      resp = await this._fetch(`${this.baseUrl}/rpc`, {
        method: "POST",
        headers,
        body: JSON.stringify(payload),
        signal: ctrl.signal,
      });
    } catch (e) {
      const why = e instanceof Error ? e.message : String(e);
      throw new DrsgError(-32000, `connection failed: ${why}`);
    } finally {
      clearTimeout(timer);
    }

    if (!resp.ok) {
      // A transport-level refusal (e.g. 403 cross-origin, 413 too large)
      // arrives as HTTP, not a JSON-RPC error — surface it uniformly.
      throw new DrsgError(-32000, `HTTP ${resp.status}: ${resp.statusText}`);
    }

    const msg = (await resp.json()) as {
      result?: unknown;
      error?: { code?: number; message?: string; data?: unknown };
    };
    if (msg.error) {
      const code = Number(msg.error.code ?? -32000);
      const Cls = code === -32001 ? DrsgAuthError : DrsgError;
      throw new Cls(code, msg.error.message ?? "error", msg.error.data);
    }
    return msg.result;
  }

  /**
   * Subscribe to the live change feed for a plane (ROADMAP §5) over a long-lived
   * WebSocket. `onChange` is called with each committed {@link ChangeEvent} until
   * you `close()` the returned {@link Subscription}. The socket auto-reconnects
   * on drop by default (best-effort — commits during a disconnect may be missed).
   *
   * Native `WebSocket` is used (browsers, Deno, Bun, Node ≥ 21); on older Node
   * pass one via `opts.WebSocket`.
   */
  watch(
    plane: string,
    onChange: (event: ChangeEvent) => void,
    opts: WatchOptions = {},
  ): Subscription {
    const WS = opts.WebSocket ?? (globalThis as { WebSocket?: typeof WebSocket }).WebSocket;
    if (typeof WS !== "function") {
      throw new Error("no global WebSocket; pass one via `WebSocket` in WatchOptions");
    }
    // http→ws, https→wss; the browser WS API can't set headers, so the token
    // rides the query string (the server reads `?token=` there).
    const url =
      this.baseUrl.replace(/^http/, "ws") +
      "/ws" +
      (this.token ? `?token=${encodeURIComponent(this.token)}` : "");

    const reconnect = opts.reconnect ?? true;
    let closed = false;
    let sock: WebSocket | null = null;
    let backoff = 500;

    const open = (): void => {
      const ws = new WS(url);
      sock = ws;
      ws.onopen = (): void => {
        backoff = 500;
        opts.onState?.(true);
        const params: Record<string, unknown> = { plane };
        if (opts.label) params.label = opts.label;
        ws.send(JSON.stringify({ jsonrpc: "2.0", method: "plane.watch", params, id: 1 }));
      };
      ws.onmessage = (ev: MessageEvent): void => {
        let msg: { method?: string; params?: ChangeEvent };
        try {
          msg = JSON.parse(typeof ev.data === "string" ? ev.data : String(ev.data));
        } catch {
          return;
        }
        if (msg.method === "plane.change" && msg.params) onChange(msg.params);
      };
      ws.onclose = (): void => {
        opts.onState?.(false);
        if (!closed && reconnect) {
          setTimeout(open, backoff);
          backoff = Math.min(backoff * 2, 10_000);
        }
      };
      ws.onerror = (): void => {
        try {
          ws.close();
        } catch {
          /* already closing */
        }
      };
    };

    open();
    return {
      close(): void {
        closed = true;
        try {
          sock?.close();
        } catch {
          /* already closed */
        }
      },
    };
  }
}
