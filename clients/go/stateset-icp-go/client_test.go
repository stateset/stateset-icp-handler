package icp

import (
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

// newTestClient spins up an httptest.Server with the given handler and
// returns a Client pointed at it. The server closes when the test ends
// via t.Cleanup — no leaked goroutines.
func newTestClient(t *testing.T, h http.HandlerFunc) *Client {
	t.Helper()
	srv := httptest.NewServer(h)
	t.Cleanup(srv.Close)
	return New(srv.URL, "test-key", "did:stateset:agent:go-test")
}

// captured snapshots a single request so a test can assert on it.
type captured struct {
	Method string
	Path   string
	Header http.Header
	Body   map[string]any
}

func capture(t *testing.T, seen *[]captured, status int, respBody any) http.HandlerFunc {
	t.Helper()
	return func(w http.ResponseWriter, r *http.Request) {
		cap := captured{
			Method: r.Method,
			Path:   r.URL.Path,
			Header: r.Header.Clone(),
		}
		if r.Body != nil {
			raw, _ := io.ReadAll(r.Body)
			if len(raw) > 0 {
				_ = json.Unmarshal(raw, &cap.Body)
			}
		}
		*seen = append(*seen, cap)
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(status)
		if respBody != nil {
			_ = json.NewEncoder(w).Encode(respBody)
		}
	}
}

// ---- Read-side --------------------------------------------------------

func TestDiscoveryHitsWellKnownAndCarriesAuthHeaders(t *testing.T) {
	var seen []captured
	c := newTestClient(t, capture(t, &seen, 200, map[string]any{
		"icp_version":  "2026-04-21",
		"service_name": "StateSet ICP Handler",
	}))
	got, err := c.Discovery()
	if err != nil {
		t.Fatalf("Discovery: %v", err)
	}
	if got["icp_version"] != "2026-04-21" {
		t.Errorf("unexpected body: %v", got)
	}
	if len(seen) != 1 {
		t.Fatalf("want 1 request, got %d", len(seen))
	}
	req := seen[0]
	if req.Method != "GET" || req.Path != "/.well-known/icp" {
		t.Errorf("unexpected route: %s %s", req.Method, req.Path)
	}
	if got := req.Header.Get("Authorization"); !strings.HasPrefix(got, "Bearer ") {
		t.Errorf("missing bearer: %q", got)
	}
	if got := req.Header.Get("ICP-Agent-Id"); got == "" {
		t.Errorf("missing ICP-Agent-Id")
	}
	if got := req.Header.Get("ICP-Request-Id"); !strings.HasPrefix(got, "req-") {
		t.Errorf("missing auto-generated ICP-Request-Id: %q", got)
	}
}

func TestGetTransactionHitsCorrectPath(t *testing.T) {
	var seen []captured
	c := newTestClient(t, capture(t, &seen, 200, map[string]any{"id": "txn_1"}))
	if _, err := c.GetTransaction("txn_1"); err != nil {
		t.Fatalf("GetTransaction: %v", err)
	}
	if seen[0].Path != "/icp/v1/transactions/txn_1" {
		t.Errorf("path = %q", seen[0].Path)
	}
}

// ---- Submit intent ----------------------------------------------------

func TestSubmitIntentSendsEnvelopeAndHeaders(t *testing.T) {
	var seen []captured
	c := newTestClient(t, capture(t, &seen, 200, map[string]any{
		"intent": "intent.quote",
		"transaction": map[string]any{
			"id":    "txn_abc",
			"state": "quoted",
		},
	}))

	resp, err := c.SubmitIntent(
		IntentEnvelope{
			Intent:  "intent.quote",
			AgentID: "did:stateset:agent:go-test",
			Params: map[string]any{
				"items": []any{map[string]any{"sku": "WIDGET", "quantity": 2}},
			},
			Context: map[string]any{"currency": "USD"},
		},
		SubmitOptions{
			MandateJWS:     "hdr.pl.sig",
			IdempotencyKey: "idem-1",
			RequestID:      "req-explicit",
			TraceID:        "trace-1",
		},
	)
	if err != nil {
		t.Fatalf("SubmitIntent: %v", err)
	}

	// Response propagated.
	if txn, _ := resp["transaction"].(map[string]any); txn["id"] != "txn_abc" {
		t.Errorf("response not propagated: %+v", resp)
	}

	// Request shape.
	if len(seen) != 1 {
		t.Fatalf("want 1 request, got %d", len(seen))
	}
	req := seen[0]
	if req.Method != "POST" || req.Path != "/icp/v1/intents" {
		t.Errorf("route = %s %s", req.Method, req.Path)
	}
	if req.Body["intent"] != "intent.quote" {
		t.Errorf("intent not serialized: %+v", req.Body)
	}
	params, _ := req.Body["params"].(map[string]any)
	if _, ok := params["items"]; !ok {
		t.Errorf("params.items missing: %+v", req.Body)
	}

	// Every optional header present.
	wantHeaders := map[string]string{
		"Icp-Mandate":         "hdr.pl.sig",
		"Icp-Idempotency-Key": "idem-1",
		"Icp-Request-Id":      "req-explicit",
		"Icp-Trace-Id":        "trace-1",
	}
	for name, want := range wantHeaders {
		if got := req.Header.Get(name); got != want {
			t.Errorf("header %s = %q, want %q", name, got, want)
		}
	}
}

func TestSubmitIntentAutoGeneratesRequestIDWhenUnset(t *testing.T) {
	var seen []captured
	c := newTestClient(t, capture(t, &seen, 200, map[string]any{}))
	_, _ = c.SubmitIntent(
		IntentEnvelope{Intent: "intent.quote", AgentID: "did:x"},
		SubmitOptions{},
	)
	if got := seen[0].Header.Get("ICP-Request-Id"); !strings.HasPrefix(got, "req-") {
		t.Errorf("auto request id missing: %q", got)
	}
}

func TestSubmitIntentOmitsUnsetOptionalHeaders(t *testing.T) {
	var seen []captured
	c := newTestClient(t, capture(t, &seen, 200, map[string]any{}))
	_, _ = c.SubmitIntent(
		IntentEnvelope{Intent: "intent.search", AgentID: "did:x"},
		SubmitOptions{},
	)
	for _, name := range []string{"ICP-Mandate", "ICP-Idempotency-Key", "ICP-Trace-Id"} {
		if got := seen[0].Header.Get(name); got != "" {
			t.Errorf("expected %s to be absent, got %q", name, got)
		}
	}
}

// ---- Error envelope ---------------------------------------------------

func TestWrappedErrorEnvelopeParsed(t *testing.T) {
	c := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(402)
		_, _ = w.Write([]byte(`{"error":{"type":"mandate_budget_exceeded","code":"mandate_budget_exceeded","message":"over","retriable":false}}`))
	})
	_, err := c.SubmitIntent(
		IntentEnvelope{Intent: "intent.buy", AgentID: "did:x"},
		SubmitOptions{},
	)
	if err == nil {
		t.Fatal("expected error")
	}
	icpErr, ok := err.(*IcpError)
	if !ok {
		t.Fatalf("expected *IcpError, got %T: %v", err, err)
	}
	if icpErr.StatusCode != 402 {
		t.Errorf("status = %d", icpErr.StatusCode)
	}
	if icpErr.Code != "mandate_budget_exceeded" {
		t.Errorf("code = %q", icpErr.Code)
	}
	if icpErr.Retriable {
		t.Errorf("retriable should be false")
	}
}

func TestFlatErrorEnvelopeParsed(t *testing.T) {
	// Some legacy paths emit a flat envelope without the outer
	// `error` wrapper. Must parse too.
	c := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(404)
		_, _ = w.Write([]byte(`{"type":"not_found","code":"resource_not_found","message":"txn_x"}`))
	})
	_, err := c.GetTransaction("txn_x")
	if err == nil {
		t.Fatal("expected error")
	}
	icpErr := err.(*IcpError)
	if icpErr.Code != "resource_not_found" {
		t.Errorf("code = %q", icpErr.Code)
	}
	if icpErr.StatusCode != 404 {
		t.Errorf("status = %d", icpErr.StatusCode)
	}
}

func TestUnparseableErrorBodyFallsBackToStatusOnly(t *testing.T) {
	c := newTestClient(t, func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(500)
		_, _ = w.Write([]byte("internal server error"))
	})
	_, err := c.GetTransaction("txn_x")
	if err == nil {
		t.Fatal("expected error")
	}
	icpErr := err.(*IcpError)
	if icpErr.StatusCode != 500 {
		t.Errorf("status = %d", icpErr.StatusCode)
	}
	if !strings.Contains(icpErr.Error(), "500") {
		t.Errorf("Error() should include status: %q", icpErr.Error())
	}
}

func TestIcpErrorStringContainsCodeAndStatus(t *testing.T) {
	e := &IcpError{StatusCode: 403, Code: "mandate_out_of_scope", Message: "nope"}
	if !strings.Contains(e.Error(), "mandate_out_of_scope") ||
		!strings.Contains(e.Error(), "403") {
		t.Errorf("Error() = %q, want code + status", e.Error())
	}
}
