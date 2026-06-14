package icp

import (
	"strings"
	"testing"
)

// Envelope-shape tests for every per-intent wrapper. Uses the same
// `captured` helper as client_test.go — each test exercises one
// wrapper, inspects the single request it emits, and asserts the
// wire intent name + params shape match the spec.

// helper: submit through a wrapper, return the captured request.
func runWrapper(t *testing.T, fn func(c *Client) (map[string]any, error)) captured {
	t.Helper()
	var seen []captured
	c := newTestClient(t, capture(t, &seen, 200, map[string]any{"ok": true}))
	if _, err := fn(c); err != nil {
		t.Fatalf("wrapper call: %v", err)
	}
	if len(seen) != 1 {
		t.Fatalf("want 1 request, got %d", len(seen))
	}
	return seen[0]
}

// helper: assert the captured body has the given wire intent.
func assertIntent(t *testing.T, req captured, want string) {
	t.Helper()
	if got, _ := req.Body["intent"].(string); got != want {
		t.Errorf("intent = %q, want %q", got, want)
	}
}

// helper: extract typed params map from a captured body.
func params(t *testing.T, req captured) map[string]any {
	t.Helper()
	p, ok := req.Body["params"].(map[string]any)
	if !ok {
		t.Fatalf("params missing or wrong type: %+v", req.Body)
	}
	return p
}

// ---- Read-side ----

func TestSearch(t *testing.T) {
	req := runWrapper(t, func(c *Client) (map[string]any, error) {
		return c.Search("widget", 10, SubmitOptions{})
	})
	assertIntent(t, req, "intent.search")
	p := params(t, req)
	if p["query"] != "widget" || p["limit"].(float64) != 10 {
		t.Errorf("params = %+v", p)
	}
}

func TestSearchOmitsEmptyFields(t *testing.T) {
	req := runWrapper(t, func(c *Client) (map[string]any, error) {
		return c.Search("", 0, SubmitOptions{})
	})
	// `params` is tagged `omitempty`, so an all-empty Search yields
	// an envelope with no `params` key at all — a stronger version
	// of "the optional fields aren't present."
	p, _ := req.Body["params"].(map[string]any)
	if _, ok := p["query"]; ok {
		t.Errorf("empty query should be omitted: %+v", p)
	}
	if _, ok := p["limit"]; ok {
		t.Errorf("zero limit should be omitted: %+v", p)
	}
}

func TestDescribeBySku(t *testing.T) {
	req := runWrapper(t, func(c *Client) (map[string]any, error) {
		return c.Describe("", "WIDGET-001", SubmitOptions{})
	})
	assertIntent(t, req, "intent.describe")
	p := params(t, req)
	if p["sku"] != "WIDGET-001" {
		t.Errorf("sku = %v", p["sku"])
	}
	if _, ok := p["product_id"]; ok {
		t.Errorf("product_id must be absent when only sku given")
	}
}

func TestDescribeRejectsBothOrNeither(t *testing.T) {
	c := New("http://example", "k", "did:x")
	if _, err := c.Describe("", "", SubmitOptions{}); err == nil {
		t.Error("Describe(\"\",\"\") must error")
	}
	if _, err := c.Describe("p1", "s1", SubmitOptions{}); err == nil {
		t.Error("Describe(both) must error")
	}
}

func TestTrack(t *testing.T) {
	req := runWrapper(t, func(c *Client) (map[string]any, error) {
		return c.Track("txn_1", SubmitOptions{})
	})
	assertIntent(t, req, "intent.track")
	if params(t, req)["transaction_id"] != "txn_1" {
		t.Errorf("transaction_id missing")
	}
}

// ---- Buy lifecycle ----

func TestQuoteEnvelopeShape(t *testing.T) {
	req := runWrapper(t, func(c *Client) (map[string]any, error) {
		return c.Quote(QuoteParams{
			Items: []map[string]any{
				{"sku": "WIDGET-001", "quantity": 2},
			},
			Buyer:        map[string]any{"first_name": "Alice"},
			ShipTo:       map[string]any{"city": "SF"},
			Currency:     "USD",
			Jurisdiction: "US-CA",
		}, SubmitOptions{MandateJWS: "jws", IdempotencyKey: "idem-1"})
	})
	assertIntent(t, req, "intent.quote")
	p := params(t, req)
	items, _ := p["items"].([]any)
	if len(items) != 1 {
		t.Errorf("items = %+v", items)
	}
	if p["buyer"] == nil || p["ship_to"] == nil {
		t.Errorf("buyer/ship_to not propagated: %+v", p)
	}
	ctx, _ := req.Body["context"].(map[string]any)
	if ctx["currency"] != "USD" || ctx["jurisdiction"] != "US-CA" {
		t.Errorf("context = %+v", ctx)
	}
	if req.Header.Get("ICP-Mandate") != "jws" {
		t.Errorf("mandate header missing")
	}
	if req.Header.Get("ICP-Idempotency-Key") != "idem-1" {
		t.Errorf("idempotency header missing")
	}
}

func TestAuthorizeAndBuyShareTransactionID(t *testing.T) {
	var seen []captured
	c := newTestClient(t, capture(t, &seen, 200, map[string]any{"ok": true}))
	_, _ = c.Authorize("txn_42", SubmitOptions{})
	_, _ = c.Buy("txn_42", map[string]any{"method": "card", "token": "tok"}, SubmitOptions{})
	if len(seen) != 2 {
		t.Fatalf("want 2 requests, got %d", len(seen))
	}
	if seen[0].Body["intent"] != "intent.authorize" {
		t.Errorf("seen[0].intent = %v", seen[0].Body["intent"])
	}
	if seen[1].Body["intent"] != "intent.buy" {
		t.Errorf("seen[1].intent = %v", seen[1].Body["intent"])
	}
	for i, req := range seen {
		if params(t, req)["transaction_id"] != "txn_42" {
			t.Errorf("seen[%d].transaction_id missing", i)
		}
	}
	buyParams := params(t, seen[1])
	if pay, _ := buyParams["payment"].(map[string]any); pay["token"] != "tok" {
		t.Errorf("payment not propagated in Buy")
	}
}

func TestPay(t *testing.T) {
	req := runWrapper(t, func(c *Client) (map[string]any, error) {
		return c.Pay("txn_7", map[string]any{"method": "stablecoin", "asset": "USDC"}, SubmitOptions{})
	})
	assertIntent(t, req, "intent.pay")
	if pay, _ := params(t, req)["payment"].(map[string]any); pay["asset"] != "USDC" {
		t.Errorf("payment not passed through")
	}
}

// ---- Post-sale ----

func TestReturnPreservesWireName(t *testing.T) {
	// Method is capital-R `Return`, wire intent is still
	// `intent.return` — proves the Go keyword doesn't collide with
	// the exported method name.
	req := runWrapper(t, func(c *Client) (map[string]any, error) {
		return c.Return(
			"txn_1",
			[]map[string]any{{"sku": "WIDGET", "quantity": 1}},
			"damaged",
			SubmitOptions{},
		)
	})
	assertIntent(t, req, "intent.return")
	p := params(t, req)
	if p["reason"] != "damaged" {
		t.Errorf("reason = %v", p["reason"])
	}
	if items, _ := p["items"].([]any); len(items) != 1 {
		t.Errorf("items = %+v", items)
	}
}

func TestRefundRequestFullVsPartial(t *testing.T) {
	// Full refund — amount absent from wire.
	req := runWrapper(t, func(c *Client) (map[string]any, error) {
		return c.RefundRequest("txn_1", nil, "", SubmitOptions{})
	})
	p := params(t, req)
	if _, ok := p["amount"]; ok {
		t.Errorf("full refund should omit amount: %+v", p)
	}

	// Partial refund — amount echoed.
	req2 := runWrapper(t, func(c *Client) (map[string]any, error) {
		return c.RefundRequest(
			"txn_1",
			map[string]any{"amount_minor": 500, "currency": "USD"},
			"partial",
			SubmitOptions{},
		)
	})
	p2 := params(t, req2)
	if amt, _ := p2["amount"].(map[string]any); amt["amount_minor"].(float64) != 500 {
		t.Errorf("partial amount missing: %+v", p2)
	}
	if p2["reason"] != "partial" {
		t.Errorf("reason missing: %+v", p2)
	}
}

func TestConfirmReceipt(t *testing.T) {
	req := runWrapper(t, func(c *Client) (map[string]any, error) {
		return c.ConfirmReceipt("txn_1", "received intact", SubmitOptions{})
	})
	assertIntent(t, req, "intent.confirm_receipt")
	p := params(t, req)
	if p["note"] != "received intact" {
		t.Errorf("note = %v", p["note"])
	}
}

func TestNegotiateRequiresExactlyOneOfProposedOrDiscount(t *testing.T) {
	c := New("http://example", "k", "did:x")
	if _, err := c.Negotiate(NegotiateParams{TransactionID: "txn_1"}, SubmitOptions{}); err == nil {
		t.Error("Negotiate with neither must error")
	}
	if _, err := c.Negotiate(NegotiateParams{
		TransactionID: "txn_1",
		ProposedTotal: map[string]any{"amount_minor": 100, "currency": "USD"},
		DiscountPct:   10.0,
	}, SubmitOptions{}); err == nil {
		t.Error("Negotiate with both must error")
	}

	req := runWrapper(t, func(c *Client) (map[string]any, error) {
		return c.Negotiate(NegotiateParams{
			TransactionID: "txn_1",
			DiscountPct:   15.0,
			Message:       "take it or leave it",
		}, SubmitOptions{})
	})
	assertIntent(t, req, "intent.negotiate")
	p := params(t, req)
	if p["discount_pct"].(float64) != 15.0 || p["message"] != "take it or leave it" {
		t.Errorf("negotiate params = %+v", p)
	}
	if _, ok := p["proposed_total"]; ok {
		t.Errorf("proposed_total must be absent when discount_pct is set")
	}
}

// ---- Subscriptions ----

func TestSubscribe(t *testing.T) {
	req := runWrapper(t, func(c *Client) (map[string]any, error) {
		return c.Subscribe(SubscribeParams{
			Items:    []map[string]any{{"sku": "COFFEE", "quantity": 1}},
			Cadence:  "monthly",
			Currency: "USD",
			Payment:  map[string]any{"method": "card", "token": "tok"},
		}, SubmitOptions{})
	})
	assertIntent(t, req, "intent.subscribe")
	p := params(t, req)
	if p["cadence"] != "monthly" {
		t.Errorf("cadence = %v", p["cadence"])
	}
	if pay, _ := p["payment"].(map[string]any); pay["token"] != "tok" {
		t.Errorf("payment not passed through")
	}
}

func TestSubscriptionLifecycleEnvelopes(t *testing.T) {
	cases := []struct {
		name string
		call func(c *Client) (map[string]any, error)
		wire string
	}{
		{"renew", func(c *Client) (map[string]any, error) { return c.Renew("sub_1", SubmitOptions{}) }, "intent.renew"},
		{"pause", func(c *Client) (map[string]any, error) { return c.Pause("sub_1", SubmitOptions{}) }, "intent.pause"},
		{"cancel", func(c *Client) (map[string]any, error) { return c.CancelSubscription("sub_1", SubmitOptions{}) }, "intent.cancel_subscription"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			req := runWrapper(t, tc.call)
			assertIntent(t, req, tc.wire)
			if params(t, req)["subscription_id"] != "sub_1" {
				t.Errorf("subscription_id missing")
			}
		})
	}
}

// ---- A2A ----

func TestA2AQuote(t *testing.T) {
	req := runWrapper(t, func(c *Client) (map[string]any, error) {
		return c.A2AQuote(A2AQuoteParams{
			PeerAgentID:   "did:stateset:agent:compute",
			Service:       map[string]any{"kind": "compute", "params": map[string]any{"gpus": 1}},
			PriceHint:     map[string]any{"amount_minor": 10000, "currency": "USD"},
			ExpiresInSecs: 300,
			ReferenceID:   "job-42",
		}, SubmitOptions{})
	})
	assertIntent(t, req, "intent.a2a_quote")
	p := params(t, req)
	if peer, _ := p["peer_agent_id"].(string); !strings.HasSuffix(peer, ":compute") {
		t.Errorf("peer_agent_id = %q", peer)
	}
	if svc, _ := p["service"].(map[string]any); svc["kind"] != "compute" {
		t.Errorf("service.kind missing")
	}
	// Wire field is `expires_in_seconds` (matches the server); the old
	// `expires_in_secs` key was silently dropped by the handler.
	if p["expires_in_seconds"].(float64) != 300 || p["reference_id"] != "job-42" {
		t.Errorf("optional params = %+v", p)
	}
}

func TestA2APayAgainstQuote(t *testing.T) {
	req := runWrapper(t, func(c *Client) (map[string]any, error) {
		return c.A2APay(A2APayParams{
			FromWallet:  "0xabc",
			PeerQuoteID: "pq_1",
			Memo:        "for job 42",
		}, SubmitOptions{})
	})
	assertIntent(t, req, "intent.a2a_pay")
	p := params(t, req)
	if p["from"] != "0xabc" || p["peer_quote_id"] != "pq_1" || p["memo"] != "for job 42" {
		t.Errorf("params = %+v", p)
	}
	if _, ok := p["peer_agent_id"]; ok {
		t.Errorf("peer_agent_id must be absent when using peer_quote_id")
	}
}

func TestA2APayDirect(t *testing.T) {
	req := runWrapper(t, func(c *Client) (map[string]any, error) {
		return c.A2APay(A2APayParams{
			FromWallet:  "0xabc",
			PeerAgentID: "did:stateset:agent:p",
			Amount:      map[string]any{"amount_minor": 5000, "currency": "USD"},
		}, SubmitOptions{})
	})
	p := params(t, req)
	if peer, _ := p["peer_agent_id"].(string); !strings.HasSuffix(peer, ":p") {
		t.Errorf("peer_agent_id = %q", peer)
	}
	if amt, _ := p["amount"].(map[string]any); amt["amount_minor"].(float64) != 5000 {
		t.Errorf("amount missing")
	}
	if _, ok := p["peer_quote_id"]; ok {
		t.Errorf("peer_quote_id must be absent on direct-pay")
	}
}

func TestA2APayRejectsBadShapes(t *testing.T) {
	c := New("http://example", "k", "did:x")
	cases := []struct {
		name string
		p    A2APayParams
	}{
		{"no wallet", A2APayParams{PeerQuoteID: "pq_1"}},
		{"both modes", A2APayParams{
			FromWallet:  "0xabc",
			PeerQuoteID: "pq_1",
			PeerAgentID: "did:x",
		}},
		{"direct without amount", A2APayParams{
			FromWallet:  "0xabc",
			PeerAgentID: "did:x",
		}},
		{"direct without agent", A2APayParams{
			FromWallet: "0xabc",
			Amount:     map[string]any{"amount_minor": 1, "currency": "USD"},
		}},
		{"empty", A2APayParams{FromWallet: "0xabc"}},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if _, err := c.A2APay(tc.p, SubmitOptions{}); err == nil {
				t.Errorf("%s must error", tc.name)
			}
		})
	}
}

// ---- Header propagation sanity ----

func TestMandateAndIdempotencyPropagateThroughAnyWrapper(t *testing.T) {
	req := runWrapper(t, func(c *Client) (map[string]any, error) {
		return c.Subscribe(SubscribeParams{
			Items:   []map[string]any{{"sku": "X", "quantity": 1}},
			Cadence: "weekly",
		}, SubmitOptions{
			MandateJWS:     "mandate.jws",
			IdempotencyKey: "idem-abc",
		})
	})
	if req.Header.Get("ICP-Mandate") != "mandate.jws" {
		t.Errorf("mandate header missing")
	}
	if req.Header.Get("ICP-Idempotency-Key") != "idem-abc" {
		t.Errorf("idempotency header missing")
	}
	if req.Header.Get("Authorization") == "" {
		t.Errorf("authorization header missing")
	}
}

// Compile-time contract: the 17 intent methods exist.
// If a method renames, this stops compiling. Cheap reminder that
// the surface is stable.
var _ = func() (out [17]func()) {
	c := &Client{}
	out[0] = func() { _, _ = c.Search("", 0, SubmitOptions{}) }
	out[1] = func() { _, _ = c.Describe("", "", SubmitOptions{}) }
	out[2] = func() { _, _ = c.Quote(QuoteParams{}, SubmitOptions{}) }
	out[3] = func() { _, _ = c.Authorize("", SubmitOptions{}) }
	out[4] = func() { _, _ = c.Buy("", nil, SubmitOptions{}) }
	out[5] = func() { _, _ = c.Pay("", nil, SubmitOptions{}) }
	out[6] = func() { _, _ = c.Track("", SubmitOptions{}) }
	out[7] = func() { _, _ = c.Return("", nil, "", SubmitOptions{}) }
	out[8] = func() { _, _ = c.RefundRequest("", nil, "", SubmitOptions{}) }
	out[9] = func() { _, _ = c.Subscribe(SubscribeParams{}, SubmitOptions{}) }
	out[10] = func() { _, _ = c.Renew("", SubmitOptions{}) }
	out[11] = func() { _, _ = c.Pause("", SubmitOptions{}) }
	out[12] = func() { _, _ = c.CancelSubscription("", SubmitOptions{}) }
	out[13] = func() { _, _ = c.A2AQuote(A2AQuoteParams{}, SubmitOptions{}) }
	out[14] = func() { _, _ = c.A2APay(A2APayParams{}, SubmitOptions{}) }
	out[15] = func() { _, _ = c.Negotiate(NegotiateParams{}, SubmitOptions{}) }
	out[16] = func() { _, _ = c.ConfirmReceipt("", "", SubmitOptions{}) }
	return out
}
