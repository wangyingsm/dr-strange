// Package drsg is a client for a dr-strange server (`drsg serve` JSON-RPC).
//
// The typed method surface lives in the generated generated.go (see
// internal/gen and `go generate ./...`); this file is the hand-written core it
// builds on. Zero dependencies beyond the standard library.
package drsg

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"strings"
	"sync/atomic"
	"time"
)

// DefaultBaseURL is used when no base URL is configured.
const DefaultBaseURL = "http://127.0.0.1:7700"

// AuthErrorCode is the JSON-RPC error code for a missing/invalid credential.
const AuthErrorCode = -32001

// Error is a JSON-RPC error returned by the server (carries the numeric Code).
type Error struct {
	Code    int             `json:"code"`
	Message string          `json:"message"`
	Data    json.RawMessage `json:"data,omitempty"`
}

func (e *Error) Error() string {
	return fmt.Sprintf("%s (code %d)", e.Message, e.Code)
}

// IsAuth reports whether this error is an authentication failure (-32001).
func (e *Error) IsAuth() bool { return e.Code == AuthErrorCode }

// IsAuthError reports whether err is a dr-strange auth failure (-32001).
//
// With no token configured server-side, only the same-origin browser UI is
// authorized — a programmatic client must present a token via WithToken or the
// DRSG_TOKEN environment variable.
func IsAuthError(err error) bool {
	var e *Error
	return errors.As(err, &e) && e.IsAuth()
}

// Client is one endpoint's worth of config plus the JSON-RPC call primitive.
// The zero value is not usable — build one with New.
type Client struct {
	baseURL string
	token   string
	http    *http.Client
	id      atomic.Int64
}

// Option configures a Client in New.
type Option func(*Client)

// WithBaseURL overrides the endpoint (default DefaultBaseURL).
func WithBaseURL(u string) Option { return func(c *Client) { c.baseURL = u } }

// WithToken sets the shared bearer token (default $DRSG_TOKEN).
func WithToken(t string) Option { return func(c *Client) { c.token = t } }

// WithHTTPClient supplies a custom *http.Client (timeouts, transport, …).
func WithHTTPClient(h *http.Client) Option { return func(c *Client) { c.http = h } }

// New builds a client. The base URL defaults to DefaultBaseURL and the token to
// the DRSG_TOKEN environment variable; override either with an Option.
func New(opts ...Option) *Client {
	c := &Client{
		baseURL: DefaultBaseURL,
		token:   os.Getenv("DRSG_TOKEN"),
		http:    &http.Client{Timeout: 30 * time.Second},
	}
	for _, o := range opts {
		o(c)
	}
	c.baseURL = strings.TrimRight(c.baseURL, "/")
	return c
}

type rpcRequest struct {
	JSONRPC string `json:"jsonrpc"`
	Method  string `json:"method"`
	ID      int64  `json:"id"`
	Params  any    `json:"params,omitempty"`
}

type rpcResponse struct {
	Result json.RawMessage `json:"result"`
	Error  *Error          `json:"error"`
}

// call sends one JSON-RPC request and unmarshals its result into out (which may
// be nil to discard it).
func (c *Client) call(ctx context.Context, method string, params, out any) error {
	body, err := json.Marshal(rpcRequest{
		JSONRPC: "2.0", Method: method, ID: c.id.Add(1), Params: params,
	})
	if err != nil {
		return &Error{Code: -32000, Message: "encode request: " + err.Error()}
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, c.baseURL+"/rpc", bytes.NewReader(body))
	if err != nil {
		return &Error{Code: -32000, Message: err.Error()}
	}
	req.Header.Set("content-type", "application/json")
	if c.token != "" {
		req.Header.Set("authorization", "Bearer "+c.token)
	}

	resp, err := c.http.Do(req)
	if err != nil {
		return &Error{Code: -32000, Message: "connection failed: " + err.Error()}
	}
	defer resp.Body.Close()

	data, err := io.ReadAll(resp.Body)
	if err != nil {
		return &Error{Code: -32000, Message: "read response: " + err.Error()}
	}
	if resp.StatusCode/100 != 2 {
		// A transport-level refusal (403 cross-origin, 413 too large) arrives as
		// HTTP, not a JSON-RPC error — surface it uniformly.
		return &Error{Code: -32000, Message: fmt.Sprintf("HTTP %d: %s", resp.StatusCode, http.StatusText(resp.StatusCode))}
	}

	var env rpcResponse
	if err := json.Unmarshal(data, &env); err != nil {
		return &Error{Code: -32000, Message: "decode response: " + err.Error()}
	}
	if env.Error != nil {
		return env.Error
	}
	if out != nil && len(env.Result) > 0 && string(env.Result) != "null" {
		if err := json.Unmarshal(env.Result, out); err != nil {
			return &Error{Code: -32000, Message: "decode result: " + err.Error()}
		}
	}
	return nil
}
