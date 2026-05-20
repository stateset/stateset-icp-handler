package main

// Covers the tool → client dispatch layer so a refactor of the Go
// client's per-intent method signatures can't silently break how
// Claude reaches the merchant. Uses httptest to stand in for the
// handler and asserts each tool routes to the correct wire intent
// with mandate attached on writes only.

import (
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	icp "github.com/stateset/stateset-icp-go"
)

// captured snapshots the JSON envelope and ICP-Mandate header of
// a single request. The fields we care about match what the handler
// would actually parse.
type captured struct {
	Path    string
	Method  string
	Mandate string
	Body    map[string]any
}

func newTestClient(t *testing.T, seen *[]captured) *icp.Client {
	t.Helper()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		c := captured{
			Path:    r.URL.Path,
			Method:  r.Method,
			Mandate: r.Header.Get("ICP-Mandate"),
		}
		if r.Body != nil {
			raw, _ := io.ReadAll(r.Body)
			_ = json.Unmarshal(raw, &c.Body)
		}
		*seen = append(*seen, c)
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"ok":true}`))
	}))
	t.Cleanup(srv.Close)
	return icp.New(srv.URL, "test-key", "did:stateset:agent:test")
}

// ---- individual tool dispatches ---------------------------------------

func TestDispatchSearchIsUnmandated(t *testing.T) {
	var seen []captured
	c := newTestClient(t, &seen)
	if _, err := dispatch(c, "mandate.jws", "icp_search",
		map[string]any{"query": "widget"},
	); err != nil {
		t.Fatalf("dispatch: %v", err)
	}
	if seen[0].Path != "/icp/v1/intents" {
		t.Errorf("path = %q", seen[0].Path)
	}
	if intent, _ := seen[0].Body["intent"].(string); intent != "intent.search" {
		t.Errorf("intent = %q", intent)
	}
	if seen[0].Mandate != "" {
		t.Errorf("read-only search must not attach mandate, got: %q", seen[0].Mandate)
	}
}

func TestDispatchQuoteAttachesMandate(t *testing.T) {
	var seen []captured
	c := newTestClient(t, &seen)
	if _, err := dispatch(c, "mandate.jws", "icp_quote",
		map[string]any{
			"items": []any{map[string]any{"sku": "WIDGET", "quantity": 2.0}},
			"buyer": map[string]any{"email": "a@b"},
		},
	); err != nil {
		t.Fatalf("dispatch: %v", err)
	}
	if intent, _ := seen[0].Body["intent"].(string); intent != "intent.quote" {
		t.Errorf("intent = %q", intent)
	}
	if seen[0].Mandate != "mandate.jws" {
		t.Errorf("mandate missing on write: %q", seen[0].Mandate)
	}
	// Items + buyer passed through to params.
	params, _ := seen[0].Body["params"].(map[string]any)
	if params["buyer"] == nil {
		t.Errorf("buyer not passed through: %+v", params)
	}
}

func TestDispatchAuthorize(t *testing.T) {
	var seen []captured
	c := newTestClient(t, &seen)
	if _, err := dispatch(c, "m", "icp_authorize",
		map[string]any{"transaction_id": "txn_42"},
	); err != nil {
		t.Fatalf("dispatch: %v", err)
	}
	if intent, _ := seen[0].Body["intent"].(string); intent != "intent.authorize" {
		t.Errorf("intent = %q", intent)
	}
	params, _ := seen[0].Body["params"].(map[string]any)
	if params["transaction_id"] != "txn_42" {
		t.Errorf("transaction_id not passed through: %+v", params)
	}
	if seen[0].Mandate != "m" {
		t.Errorf("mandate missing on write")
	}
}

func TestDispatchBuy(t *testing.T) {
	var seen []captured
	c := newTestClient(t, &seen)
	_, err := dispatch(c, "m", "icp_buy", map[string]any{
		"transaction_id": "txn_42",
		"payment":        map[string]any{"method": "card", "token": "tok"},
	})
	if err != nil {
		t.Fatalf("dispatch: %v", err)
	}
	if intent, _ := seen[0].Body["intent"].(string); intent != "intent.buy" {
		t.Errorf("intent = %q", intent)
	}
	params, _ := seen[0].Body["params"].(map[string]any)
	pay, _ := params["payment"].(map[string]any)
	if pay["token"] != "tok" {
		t.Errorf("payment not passed through: %+v", pay)
	}
	if seen[0].Mandate != "m" {
		t.Errorf("mandate missing on write")
	}
}

func TestDispatchTrackIsUnmandated(t *testing.T) {
	var seen []captured
	c := newTestClient(t, &seen)
	if _, err := dispatch(c, "mandate.jws", "icp_track",
		map[string]any{"transaction_id": "txn_1"},
	); err != nil {
		t.Fatalf("dispatch: %v", err)
	}
	if intent, _ := seen[0].Body["intent"].(string); intent != "intent.track" {
		t.Errorf("intent = %q", intent)
	}
	if seen[0].Mandate != "" {
		t.Errorf("read-only track must not attach mandate, got: %q", seen[0].Mandate)
	}
}

func TestDispatchUnknownToolRejected(t *testing.T) {
	var seen []captured
	c := newTestClient(t, &seen)
	_, err := dispatch(c, "m", "icp_nonexistent", map[string]any{})
	if err == nil {
		t.Fatal("expected error for unknown tool")
	}
	if !strings.Contains(err.Error(), "unknown tool") {
		t.Errorf("error message = %q", err.Error())
	}
	if len(seen) != 0 {
		t.Errorf("unknown tool should not emit an HTTP request, got %d", len(seen))
	}
}

// ---- compactJSON ------------------------------------------------------

func TestCompactJSONTruncatesLongInput(t *testing.T) {
	big := map[string]any{
		"a": strings.Repeat("x", 200),
	}
	got := compactJSON(big)
	if len(got) > 120 {
		t.Errorf("compactJSON did not truncate: len=%d", len(got))
	}
	if !strings.HasSuffix(got, "...") {
		t.Errorf("compactJSON truncation should end in '...': %q", got)
	}
}

func TestCompactJSONKeepsShortInput(t *testing.T) {
	got := compactJSON(map[string]any{"k": "v"})
	if got != `{"k":"v"}` {
		t.Errorf("compactJSON short = %q", got)
	}
}

// ---- quoteParamsFrom --------------------------------------------------

func TestQuoteParamsFromPicksTypedFields(t *testing.T) {
	p := quoteParamsFrom(map[string]any{
		"items": []any{
			map[string]any{"sku": "X", "quantity": 1.0},
		},
		"buyer":    map[string]any{"first_name": "A"},
		"ship_to":  map[string]any{"city": "SF"},
		"currency": "USD",
	})
	if len(p.Items) != 1 {
		t.Errorf("items = %+v", p.Items)
	}
	if p.Buyer["first_name"] != "A" {
		t.Errorf("buyer = %+v", p.Buyer)
	}
	if p.Currency != "USD" {
		t.Errorf("currency = %q", p.Currency)
	}
}
