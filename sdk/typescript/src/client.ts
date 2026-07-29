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
}
