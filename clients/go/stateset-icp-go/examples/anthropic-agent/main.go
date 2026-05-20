// anthropic-agent — Claude drives an ICP merchant end-to-end via
// tool use.
//
// Parallels clients/python/examples/anthropic_agent.py but hits the
// Anthropic Messages API directly over HTTP instead of taking the
// anthropic-sdk-go dependency. That keeps the top-level module's
// "zero non-stdlib deps" discipline intact and lets readers see the
// exact JSON wire format Claude consumes — nothing hidden behind an
// SDK wrapper.
//
// Registers five ICP intents as Anthropic tools
// (icp_search / icp_quote / icp_authorize / icp_buy / icp_track) and
// runs the tool_use / tool_result loop until the model emits an
// end_turn. Mandate signed once at startup and reused across every
// write; reads go unmandated.
//
// Run locally:
//
//	# Terminal 1: handler
//	cd ../../../..
//	cargo run --release
//
//	# Terminal 2: this demo
//	cd clients/go/stateset-icp-go
//	export ANTHROPIC_API_KEY=sk-ant-...
//	go run ./examples/anthropic-agent
//
// Override via env: ICP_URL, ICP_API_KEY, ICP_AGENT_ID, ANTHROPIC_MODEL,
// ICP_PROMPT (the natural-language instruction the model drives from).
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"time"

	icp "github.com/stateset/stateset-icp-go"
)

// --------------------------------------------------------------------------
// Config
// --------------------------------------------------------------------------

const anthropicURL = "https://api.anthropic.com/v1/messages"

// Pinned Messages API version. Bump deliberately when adopting new
// features; a future-dated version string here doesn't retroactively
// break old code.
const anthropicVersion = "2023-06-01"

const defaultPrompt = "I'm shopping for a friend. Please quote two WIDGET-001s at " +
	"roughly $29.99 each, shipped to Alice Smith, 1 Market St, San Francisco, " +
	"CA 94105, US. If the total is reasonable, authorize and buy with a test " +
	"card (method=card, token=tok_demo, last_digits=4242, brand=visa), then " +
	"tell me the order id and receipt jti."

// claude-sonnet-4-6 is a good default for tool use: fast enough for an
// interactive loop, cheap enough for a demo, capable enough to run a
// multi-step plan without prompting tricks.
const defaultModel = "claude-sonnet-4-6"

func env(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}

// --------------------------------------------------------------------------
// Tool catalog
// --------------------------------------------------------------------------
//
// Five tools cover the lifecycle Claude needs to drive: search → quote →
// authorize → buy → track. Input schemas are kept minimal so dispatch can
// hand tool_input straight into the Go client's typed params structs.

var tools = []map[string]any{
	{
		"name": "icp_search",
		"description": "Search the merchant's catalog. Use first when the user names " +
			"an item by description rather than SKU.",
		"input_schema": map[string]any{
			"type": "object",
			"properties": map[string]any{
				"query": map[string]any{"type": "string"},
				"limit": map[string]any{"type": "integer"},
			},
			"required": []string{"query"},
		},
	},
	{
		"name": "icp_quote",
		"description": "Request a priced quote for a basket. `items` is an array of " +
			"{sku, quantity, unit_price_hint?}. `buyer` and `ship_to` are optional.",
		"input_schema": map[string]any{
			"type": "object",
			"properties": map[string]any{
				"items":    map[string]any{"type": "array", "items": map[string]any{"type": "object"}},
				"buyer":    map[string]any{"type": "object"},
				"ship_to":  map[string]any{"type": "object"},
				"currency": map[string]any{"type": "string", "default": "USD"},
			},
			"required": []string{"items"},
		},
	},
	{
		"name":        "icp_authorize",
		"description": "Authorize a transaction previously produced by icp_quote. Pass the transaction_id from the quote's response.",
		"input_schema": map[string]any{
			"type":       "object",
			"properties": map[string]any{"transaction_id": map[string]any{"type": "string"}},
			"required":   []string{"transaction_id"},
		},
	},
	{
		"name":        "icp_buy",
		"description": "Capture payment + place the order. Call after icp_authorize. `payment` is {method, token, last_digits, brand}.",
		"input_schema": map[string]any{
			"type": "object",
			"properties": map[string]any{
				"transaction_id": map[string]any{"type": "string"},
				"payment":        map[string]any{"type": "object"},
			},
			"required": []string{"transaction_id", "payment"},
		},
	},
	{
		"name":        "icp_track",
		"description": "Check fulfillment + shipment status for a transaction.",
		"input_schema": map[string]any{
			"type":       "object",
			"properties": map[string]any{"transaction_id": map[string]any{"type": "string"}},
			"required":   []string{"transaction_id"},
		},
	},
}

// --------------------------------------------------------------------------
// Tool dispatch
// --------------------------------------------------------------------------

// dispatch routes a Claude tool_use call to the Go ICP client. Mandate
// attaches to writes only; reads go unmandated — matches ICP_SPEC §6.1.
func dispatch(c *icp.Client, mandate, name string, in map[string]any) (any, error) {
	opts := icp.SubmitOptions{MandateJWS: mandate}
	switch name {
	case "icp_search":
		query, _ := in["query"].(string)
		limit := 0
		if v, ok := in["limit"].(float64); ok {
			limit = int(v)
		}
		return c.Search(query, limit, icp.SubmitOptions{}) // read — no mandate
	case "icp_quote":
		return c.Quote(quoteParamsFrom(in), opts)
	case "icp_authorize":
		txnID, _ := in["transaction_id"].(string)
		return c.Authorize(txnID, opts)
	case "icp_buy":
		txnID, _ := in["transaction_id"].(string)
		payment, _ := in["payment"].(map[string]any)
		return c.Buy(txnID, payment, opts)
	case "icp_track":
		txnID, _ := in["transaction_id"].(string)
		return c.Track(txnID, icp.SubmitOptions{}) // read
	default:
		return nil, fmt.Errorf("unknown tool: %s", name)
	}
}

func quoteParamsFrom(in map[string]any) icp.QuoteParams {
	p := icp.QuoteParams{}
	if raw, ok := in["items"].([]any); ok {
		for _, it := range raw {
			if m, ok := it.(map[string]any); ok {
				p.Items = append(p.Items, m)
			}
		}
	}
	if m, ok := in["buyer"].(map[string]any); ok {
		p.Buyer = m
	}
	if m, ok := in["ship_to"].(map[string]any); ok {
		p.ShipTo = m
	}
	if s, ok := in["currency"].(string); ok {
		p.Currency = s
	}
	return p
}

// --------------------------------------------------------------------------
// Anthropic Messages API — minimal HTTP client
// --------------------------------------------------------------------------

// anthropicMessage is the minimum shape we care about parsing out of
// the Messages API response. Anthropic adds fields over time; this
// keeps us tolerant of additive changes by decoding only what we need.
type anthropicMessage struct {
	StopReason string           `json:"stop_reason"`
	Content    []map[string]any `json:"content"`
}

// callAnthropic posts the given messages + tools to the Messages API
// and returns the decoded response. All fields on the request side
// are typed as map[string]any so we can pass through the exact JSON
// the API expects (text blocks, tool_use blocks, tool_result blocks)
// without pre-committing to a schema that'll drift.
func callAnthropic(ctx context.Context, apiKey, model string, messages []map[string]any) (*anthropicMessage, error) {
	reqBody := map[string]any{
		"model":      model,
		"max_tokens": 1024,
		"tools":      tools,
		"messages":   messages,
	}
	body, err := json.Marshal(reqBody)
	if err != nil {
		return nil, fmt.Errorf("marshal request: %w", err)
	}

	req, err := http.NewRequestWithContext(ctx, "POST", anthropicURL, bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("x-api-key", apiKey)
	req.Header.Set("anthropic-version", anthropicVersion)

	client := &http.Client{Timeout: 90 * time.Second}
	resp, err := client.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	respBody, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, err
	}
	if resp.StatusCode >= 400 {
		return nil, fmt.Errorf("anthropic API %d: %s", resp.StatusCode, string(respBody))
	}

	var out anthropicMessage
	if err := json.Unmarshal(respBody, &out); err != nil {
		return nil, fmt.Errorf("decode response: %w (body=%s)", err, string(respBody))
	}
	return &out, nil
}

// --------------------------------------------------------------------------
// Conversation loop
// --------------------------------------------------------------------------

func run(ctx context.Context, prompt, apiKey, model string, c *icp.Client, mandate string) error {
	messages := []map[string]any{
		{"role": "user", "content": prompt},
	}
	fmt.Printf("\n[user] %s\n\n", prompt)

	// Defensive cap — a well-behaved agent converges in a handful
	// of turns; anything past this is usually a schema mismatch.
	for turn := 0; turn < 20; turn++ {
		resp, err := callAnthropic(ctx, apiKey, model, messages)
		if err != nil {
			return err
		}

		for _, block := range resp.Content {
			switch block["type"] {
			case "text":
				if text, _ := block["text"].(string); text != "" {
					fmt.Printf("[assistant] %s\n", text)
				}
			case "tool_use":
				name, _ := block["name"].(string)
				input, _ := block["input"].(map[string]any)
				fmt.Printf("[tool_use] %s(%s)\n", name, compactJSON(input))
			}
		}

		// Append the assistant's full content so the next turn sees it.
		messages = append(messages, map[string]any{
			"role":    "assistant",
			"content": resp.Content,
		})

		if resp.StopReason != "tool_use" {
			return nil
		}

		// Dispatch every tool_use block in order and build the
		// tool_result payload for the next user turn.
		var toolResults []map[string]any
		for _, block := range resp.Content {
			if block["type"] != "tool_use" {
				continue
			}
			id, _ := block["id"].(string)
			name, _ := block["name"].(string)
			input, _ := block["input"].(map[string]any)

			result, err := dispatch(c, mandate, name, input)
			if err != nil {
				// Surface handler-side errors with structured
				// code + status so the model can reason about
				// them instead of just seeing "error".
				var icpErr *icp.IcpError
				if errors.As(err, &icpErr) {
					content, _ := json.Marshal(map[string]any{
						"error":   true,
						"code":    icpErr.Code,
						"status":  icpErr.StatusCode,
						"message": icpErr.Message,
					})
					toolResults = append(toolResults, map[string]any{
						"type":        "tool_result",
						"tool_use_id": id,
						"content":     string(content),
						"is_error":    true,
					})
					fmt.Printf("[tool_result] %s → %s (%d)\n", name, icpErr.Code, icpErr.StatusCode)
				} else {
					content, _ := json.Marshal(map[string]any{
						"error":   true,
						"message": err.Error(),
					})
					toolResults = append(toolResults, map[string]any{
						"type":        "tool_result",
						"tool_use_id": id,
						"content":     string(content),
						"is_error":    true,
					})
					fmt.Printf("[tool_result] %s → error: %v\n", name, err)
				}
				continue
			}
			content, _ := json.Marshal(result)
			toolResults = append(toolResults, map[string]any{
				"type":        "tool_result",
				"tool_use_id": id,
				"content":     string(content),
			})
			fmt.Printf("[tool_result] %s → ok\n", name)
		}

		messages = append(messages, map[string]any{
			"role":    "user",
			"content": toolResults,
		})
	}

	return errors.New("iteration cap reached — giving up (likely a schema mismatch)")
}

// compactJSON renders a map as a single-line JSON string, truncated
// for readability in the trace output. Used purely for the
// `[tool_use] foo(...)` line; not sent on the wire.
func compactJSON(v any) string {
	b, err := json.Marshal(v)
	if err != nil {
		return "<err>"
	}
	s := string(b)
	if len(s) > 120 {
		return s[:117] + "..."
	}
	return s
}

// --------------------------------------------------------------------------
// Entry point
// --------------------------------------------------------------------------

func main() {
	apiKey := os.Getenv("ANTHROPIC_API_KEY")
	if apiKey == "" {
		log.Fatal("ANTHROPIC_API_KEY is not set")
	}

	icpURL := env("ICP_URL", "http://localhost:8082")
	tenantKey := env("ICP_API_KEY", "icp_demo_key_123")
	agentID := env("ICP_AGENT_ID", "did:stateset:agent:claude-demo-go")
	model := env("ANTHROPIC_MODEL", defaultModel)
	prompt := env("ICP_PROMPT", defaultPrompt)

	// Mint a fresh did:key + mandate covering the buy lifecycle.
	// Budget caps what Claude can spend across the whole session;
	// set high enough that the demo doesn't bounce off it, low
	// enough that a runaway agent cannot drain a real account.
	kp, err := icp.GenerateKeyPair()
	if err != nil {
		log.Fatalf("GenerateKeyPair: %v", err)
	}
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
	mandate, err := icp.SignMandate(payload, kp)
	if err != nil {
		log.Fatalf("SignMandate: %v", err)
	}

	c := icp.New(icpURL, tenantKey, agentID)

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
	defer cancel()

	if err := run(ctx, prompt, apiKey, model, c, mandate); err != nil {
		log.Fatal(err)
	}
}
