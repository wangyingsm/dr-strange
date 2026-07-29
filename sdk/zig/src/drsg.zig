//! dr-strange Zig client — a thin idiomatic binding over the C client
//! (sdk/c), which is generated from the OpenRPC schema. The generated typed
//! functions and json-c helpers are re-exported as `drsg.c`; this module adds
//! a small `Client` wrapper (RAII init/deinit, Zig error unions).
//!
//! HTTP is libcurl and JSON is json-c, both linked from the C library.
const std = @import("std");

pub const c = @cImport({
    @cInclude("stdlib.h");
    @cInclude("drsg.h");
});

/// Error code for a missing/invalid credential.
pub const AUTH_ERROR_CODE = c.DRSG_AUTH_ERROR_CODE;

pub const CallError = error{
    /// The server rejected the credential (code -32001).
    Unauthorized,
    /// Any other JSON-RPC or transport failure (see `lastError`).
    RpcFailed,
};

/// A dr-strange server client. Wraps the C `drsg_client`; call the generated
/// `drsg.c.drsg_<method>` functions with `.handle` and `&.err`, or the generic
/// `call` below. Every returned `*c.json_object` is owned by the caller
/// (`c.json_object_put`).
pub const Client = struct {
    handle: *c.drsg_client,
    err: c.drsg_error = std.mem.zeroes(c.drsg_error),

    /// base_url null -> http://127.0.0.1:7700; token null -> $DRSG_TOKEN.
    pub fn init(base_url: [*c]const u8, token: [*c]const u8) error{InitFailed}!Client {
        const handle = c.drsg_client_new(base_url, token) orelse return error.InitFailed;
        return .{ .handle = handle };
    }

    pub fn deinit(self: *Client) void {
        c.drsg_client_free(self.handle);
    }

    /// Low-level JSON-RPC call. `params` is borrowed (may be null). Returns an
    /// owned json_object (may be null for a JSON null result).
    pub fn call(self: *Client, method: [*c]const u8, params: ?*c.json_object) CallError!?*c.json_object {
        var result: ?*c.json_object = null;
        const rc = c.drsg_call(self.handle, method, params, &result, &self.err);
        if (rc != 0) {
            return if (c.drsg_is_auth_error(&self.err) != 0) error.Unauthorized else error.RpcFailed;
        }
        return result;
    }

    /// Details of the last failed call.
    pub fn lastError(self: *const Client) c.drsg_error {
        return self.err;
    }
};
