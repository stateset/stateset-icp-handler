// Package icp is the Go client for StateSet ICP handlers.
//
// Handwritten from /openapi.json and docs/specification/ICP_SPEC.md.
// No imports from the Rust reference handler; zero non-stdlib deps.
// If this client keeps working as the spec evolves, the spec is
// genuinely implementable across languages.
//
// This is the MVP (v0.0.1). It ships the transport layer, typed
// errors, and the low-level SubmitIntent call; ergonomic wrappers
// for the 17 ICP-Full intents live in a follow-up release.
package icp

import (
	"bytes"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"
)

// Client is a synchronous ICP client.
//
// Construct with New; override the HTTP transport or timeout via the
// optional Config. Safe for concurrent use by multiple goroutines —
// the underlying http.Client is itself concurrent-safe.
type Client struct {
	baseURL string
	apiKey  string
	agentID string
	http    *http.Client
}

// Config tunes optional Client behavior. Zero values select sensible
// defaults: 30s timeout, http.DefaultTransport.
type Config struct {
	Timeout   time.Duration
	Transport http.RoundTripper
}

// New returns a Client pointed at baseURL.
//
// baseURL is the handler's public URL (e.g. "http://localhost:8082"),
// trimmed of trailing slashes. apiKey is the tenant bearer token;
// agentID is the caller's DID, sent on every request as ICP-Agent-Id.
func New(baseURL, apiKey, agentID string, cfg ...Config) *Client {
	var c Config
	if len(cfg) > 0 {
		c = cfg[0]
	}
	timeout := c.Timeout
	if timeout == 0 {
		timeout = 30 * time.Second
	}
	return &Client{
		baseURL: strings.TrimRight(baseURL, "/"),
		apiKey:  apiKey,
		agentID: agentID,
		http: &http.Client{
			Transport: c.Transport, // nil → http.DefaultTransport
			Timeout:   timeout,
		},
	}
}

// IntentEnvelope is the canonical body posted to /icp/v1/intents
// (ICP_SPEC §7.1). Use map[string]any for Params so callers can pass
// any spec-defined shape without the client pre-committing to a
// per-intent struct for every one of 17 intents.
type IntentEnvelope struct {
	Intent  string         `json:"intent"`
	AgentID string         `json:"agent_id"`
	Params  map[string]any `json:"params,omitempty"`
	Context map[string]any `json:"context,omitempty"`
}

// SubmitOptions carries the optional headers that submit_intent
// accepts. All fields are optional; a zero-valued struct just sends
// the envelope with auth + agent-id.
type SubmitOptions struct {
	// MandateJWS is attached as the ICP-Mandate header. Required
	// for scope-gated intents when the handler has
	// ICP_REQUIRE_MANDATE=true (the default).
	MandateJWS string
	// IdempotencyKey attaches ICP-Idempotency-Key. Same key + same
	// JCS-canonicalized body replays the original response.
	IdempotencyKey string
	// RequestID attaches ICP-Request-Id. Auto-generated if empty so
	// every outbound call has a correlation id for handler logs.
	RequestID string
	// TraceID attaches ICP-Trace-Id for distributed tracing.
	TraceID string
}

// IcpError is the structured error envelope returned on 4xx/5xx
// (ICP_SPEC §12). Implements the error interface.
type IcpError struct {
	StatusCode int    `json:"-"`
	Type       string `json:"type,omitempty"`
	Code       string `json:"code,omitempty"`
	Message    string `json:"message,omitempty"`
	Param      string `json:"param,omitempty"`
	IntentID   string `json:"intent_id,omitempty"`
	Retriable  bool   `json:"retriable,omitempty"`
	DocsURL    string `json:"docs_url,omitempty"`
}

func (e *IcpError) Error() string {
	code := e.Code
	if code == "" {
		code = e.Type
	}
	if code == "" {
		code = "icp_error"
	}
	return fmt.Sprintf("%s (%d): %s", code, e.StatusCode, e.Message)
}

// Discovery returns the handler's /.well-known/icp document as a
// free-form map. The shape matches the DiscoveryDocument in the Rust
// handler's src/discovery.rs but we keep it untyped here so a
// minor-version field addition on the handler side doesn't break
// existing clients.
func (c *Client) Discovery() (map[string]any, error) {
	return c.getJSON("/.well-known/icp")
}

// JWKS returns the handler's receipt-signing verifying keys.
func (c *Client) JWKS() (map[string]any, error) {
	return c.getJSON("/.well-known/icp/jwks.json")
}

// GetTransaction looks up a transaction aggregate by id.
func (c *Client) GetTransaction(id string) (map[string]any, error) {
	return c.getJSON("/icp/v1/transactions/" + id)
}

// GetSubscription looks up a subscription aggregate by id.
func (c *Client) GetSubscription(id string) (map[string]any, error) {
	return c.getJSON("/icp/v1/subscriptions/" + id)
}

// GetPeerQuote looks up a peer quote aggregate by id.
func (c *Client) GetPeerQuote(id string) (map[string]any, error) {
	return c.getJSON("/icp/v1/peer_quotes/" + id)
}

// GetReceipt fetches a signed receipt by jti.
func (c *Client) GetReceipt(jti string) (map[string]any, error) {
	return c.getJSON("/icp/v1/receipts/" + jti)
}

// GetMandateUsage returns the current spend tally for a mandate.
func (c *Client) GetMandateUsage(jti string) (map[string]any, error) {
	return c.getJSON("/icp/v1/mandates/" + jti + "/usage")
}

// SubmitIntent posts the envelope to /icp/v1/intents and returns the
// handler's response body as a map. Use this directly for any of the
// 17 ICP-Full intents; per-intent helpers are a v0.0.2 concern.
func (c *Client) SubmitIntent(env IntentEnvelope, opts SubmitOptions) (map[string]any, error) {
	body, err := json.Marshal(env)
	if err != nil {
		return nil, fmt.Errorf("marshal envelope: %w", err)
	}
	req, err := http.NewRequest("POST", c.baseURL+"/icp/v1/intents", bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	c.applyAuthHeaders(req)
	if opts.MandateJWS != "" {
		req.Header.Set("ICP-Mandate", opts.MandateJWS)
	}
	if opts.IdempotencyKey != "" {
		req.Header.Set("ICP-Idempotency-Key", opts.IdempotencyKey)
	}
	requestID := opts.RequestID
	if requestID == "" {
		requestID = autoRequestID()
	}
	req.Header.Set("ICP-Request-Id", requestID)
	if opts.TraceID != "" {
		req.Header.Set("ICP-Trace-Id", opts.TraceID)
	}
	return c.do(req)
}

// --- transport helpers ---------------------------------------------

func (c *Client) getJSON(path string) (map[string]any, error) {
	req, err := http.NewRequest("GET", c.baseURL+path, nil)
	if err != nil {
		return nil, err
	}
	c.applyAuthHeaders(req)
	req.Header.Set("ICP-Request-Id", autoRequestID())
	return c.do(req)
}

func (c *Client) applyAuthHeaders(req *http.Request) {
	req.Header.Set("Authorization", "Bearer "+c.apiKey)
	req.Header.Set("ICP-Agent-Id", c.agentID)
	req.Header.Set("Accept", "application/json")
}

func (c *Client) do(req *http.Request) (map[string]any, error) {
	resp, err := c.http.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, fmt.Errorf("read body: %w", err)
	}
	if resp.StatusCode >= 400 {
		return nil, parseError(resp.StatusCode, body)
	}
	if len(body) == 0 {
		return map[string]any{}, nil
	}
	var out map[string]any
	if err := json.Unmarshal(body, &out); err != nil {
		return nil, fmt.Errorf("decode response: %w (body=%q)", err, string(body))
	}
	return out, nil
}

func parseError(status int, body []byte) error {
	// The handler wraps errors as `{"error": {...}}`; older paths
	// return the flat error. Handle both.
	var wrapper struct {
		Error *IcpError `json:"error"`
	}
	if err := json.Unmarshal(body, &wrapper); err == nil && wrapper.Error != nil {
		wrapper.Error.StatusCode = status
		return wrapper.Error
	}
	var flat IcpError
	if err := json.Unmarshal(body, &flat); err == nil && (flat.Code != "" || flat.Type != "" || flat.Message != "") {
		flat.StatusCode = status
		return &flat
	}
	// No structured body — fall back to a minimal IcpError so
	// callers can still branch on HTTP status.
	return &IcpError{StatusCode: status, Message: string(body)}
}

func autoRequestID() string {
	var b [8]byte
	_, _ = rand.Read(b[:])
	return "req-" + hex.EncodeToString(b[:])
}
