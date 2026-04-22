// buyflow — end-to-end demo against a running StateSet ICP handler.
//
// Mirrors `clients/python/examples/buy_flow.py`: mint a did:key, sign
// a mandate, discover the handler, run quote → authorize → buy,
// re-fetch the receipt by jti, and check mandate usage.
//
// Run locally:
//
//	# Terminal 1: handler
//	cd ../../../..
//	cargo run --release
//
//	# Terminal 2: demo
//	cd clients/go/stateset-icp-go
//	go run ./examples/buyflow
//
// Override via env: ICP_URL, ICP_API_KEY, ICP_AGENT_ID.
package main

import (
	"errors"
	"fmt"
	"log"
	"os"

	icp "github.com/stateset/stateset-icp-go"
)

func env(key, fallback string) string {
	if got := os.Getenv(key); got != "" {
		return got
	}
	return fallback
}

func main() {
	url := env("ICP_URL", "http://localhost:8082")
	apiKey := env("ICP_API_KEY", "icp_demo_key_123")
	agentID := env("ICP_AGENT_ID", "did:stateset:agent:demo-go")

	// 1. Mint a fresh did:key. In production the issuer would be a
	// persistent principal (did:web pointing to a key registry);
	// did:key is self-contained and great for demos — the handler
	// can resolve it from the string alone.
	kp, err := icp.GenerateKeyPair()
	if err != nil {
		log.Fatalf("GenerateKeyPair: %v", err)
	}
	fmt.Printf("buyer did = %s\n", kp.DID)

	// 2. Build + sign a mandate that covers the three intents the
	// buy flow needs. Budget caps total spend across the whole
	// conversation — the handler rejects anything over it.
	payload, err := icp.NewMandatePayload(icp.MandateOpts{
		Issuer:            kp.DID,
		Subject:           agentID,
		Scope:             []string{"quote", "authorize", "buy"},
		BudgetCurrency:    "USD",
		BudgetAmountMinor: 50_000, // $500 ceiling
		ValidForSecs:      600,
	})
	if err != nil {
		log.Fatalf("NewMandatePayload: %v", err)
	}
	mandateJWS, err := icp.SignMandate(payload, kp)
	if err != nil {
		log.Fatalf("SignMandate: %v", err)
	}
	mandateJTI, _ := payload["jti"].(string)
	fmt.Printf("mandate jti = %s\n", mandateJTI)

	c := icp.New(url, apiKey, agentID)

	// 3. Discovery — confirms the handler is live and names its
	// conformance tier. Both icp-core and icp-full accept this flow.
	disco, err := c.Discovery()
	if err != nil {
		exit("discovery", err)
	}
	tier := "<unknown>"
	if conf, _ := disco["conformance"].(map[string]any); conf != nil {
		if t, _ := conf["tier"].(string); t != "" {
			tier = t
		}
	}
	serviceName, _ := disco["service_name"].(string)
	fmt.Printf("handler: %s (tier=%s)\n", serviceName, tier)

	// 4. Quote — prices a basket without committing to buy.
	quote, err := c.Quote(
		icp.QuoteParams{
			Items: []map[string]any{
				{
					"sku":      "WIDGET-001",
					"quantity": 2,
					"unit_price_hint": map[string]any{
						"amount_minor": 2999, "currency": "USD",
					},
				},
			},
			Buyer: map[string]any{
				"first_name": "Alice",
				"email":      "alice@example.com",
			},
			ShipTo: map[string]any{
				"name":        "Alice Smith",
				"line_one":    "1 Market St",
				"city":        "San Francisco",
				"state":       "CA",
				"postal_code": "94105",
				"country":     "US",
			},
			Currency:     "USD",
			Jurisdiction: "US-CA",
		},
		icp.SubmitOptions{MandateJWS: mandateJWS},
	)
	if err != nil {
		exit("quote", err)
	}
	txnID := stringField(quote, "transaction", "id")
	state := stringField(quote, "transaction", "state")
	total := totalDisplay(quote)
	fmt.Printf("quoted   txn=%s state=%s total=%s\n", txnID, state, total)

	// 5. Authorize — reserves funds on the payment method, if any.
	auth, err := c.Authorize(txnID, icp.SubmitOptions{MandateJWS: mandateJWS})
	if err != nil {
		exit("authorize", err)
	}
	fmt.Printf("auth'd   txn=%s state=%s\n", txnID, stringField(auth, "transaction", "state"))

	// 6. Buy — captures payment and emits an order. The response
	// carries a signed receipt (compact JWS) that a counterparty
	// can verify against /.well-known/icp/jwks.json without calling
	// us.
	buy, err := c.Buy(
		txnID,
		map[string]any{
			"method":      "card",
			"token":       "tok_demo",
			"last_digits": "4242",
			"brand":       "visa",
		},
		icp.SubmitOptions{MandateJWS: mandateJWS},
	)
	if err != nil {
		exit("buy", err)
	}
	orderID := stringField(buy, "order", "id")
	receiptJTI := stringField(buy, "receipt", "jti")
	receiptKID := stringField(buy, "receipt", "kid")
	fmt.Printf("bought   order=%s state=%s\n", orderID, stringField(buy, "transaction", "state"))
	fmt.Printf("receipt  jti=%s kid=%s\n", receiptJTI, receiptKID)

	// 7. Round-trip the receipt by jti — proves the handler persists
	// signed receipts and would survive a restart.
	fetched, err := c.GetReceipt(receiptJTI)
	if err != nil {
		exit("get_receipt", err)
	}
	if got, _ := fetched["jws"].(string); got != "" {
		fmt.Printf("receipt  fetched jws prefix=%s…\n", firstN(got, 40))
	}

	// 8. Mandate usage — shows how much of the $500 ceiling this
	// flow consumed. Client-side budgets can be enforced off this.
	usage, err := c.GetMandateUsage(mandateJTI)
	if err != nil {
		exit("get_mandate_usage", err)
	}
	if spent, ok := usage["spent_minor"]; ok {
		fmt.Printf("mandate  spent_minor=%v\n", spent)
	}
}

// --- small printing helpers --------------------------------------------

// stringField pulls a nested string field out of a generic response
// map; returns "" if any hop is missing or wrongly-typed. Keeps the
// main flow readable — raw `.(map[string]any)[...]` chains don't.
func stringField(m map[string]any, path ...string) string {
	cur := m
	for i, key := range path {
		if i == len(path)-1 {
			if s, _ := cur[key].(string); s != "" {
				return s
			}
			return ""
		}
		next, _ := cur[key].(map[string]any)
		if next == nil {
			return ""
		}
		cur = next
	}
	return ""
}

// totalDisplay extracts `.transaction.totals.total` and renders it as
// "USD 59.98"-style text. Falls back to "?" if the shape isn't what
// we expect so the demo doesn't crash on a partial response.
func totalDisplay(resp map[string]any) string {
	txn, _ := resp["transaction"].(map[string]any)
	if txn == nil {
		return "?"
	}
	totals, _ := txn["totals"].(map[string]any)
	if totals == nil {
		return "?"
	}
	total, _ := totals["total"].(map[string]any)
	if total == nil {
		return "?"
	}
	ccy, _ := total["currency"].(string)
	// amount_minor deserializes as float64 via encoding/json — safe
	// to truncate for display purposes.
	amt, _ := total["amount_minor"].(float64)
	return fmt.Sprintf("%s %.2f", ccy, amt/100)
}

// firstN returns the first n characters of s, or s itself if shorter.
// Byte-level, fine for ASCII JWS prefixes.
func firstN(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n]
}

// exit prints a structured failure + the step that hit it, then kills
// the process with a non-zero status. Separates handler errors (which
// carry useful spec info via *IcpError) from other errors.
func exit(step string, err error) {
	var icpErr *icp.IcpError
	if errors.As(err, &icpErr) {
		log.Fatalf("%s: handler rejected: %s (%d) — %s",
			step, icpErr.Code, icpErr.StatusCode, icpErr.Message)
	}
	log.Fatalf("%s: %v", step, err)
}
