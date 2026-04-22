// listen — tail the handler's SSE event stream and print each event
// to stdout.
//
// Pairs with ../buyflow: one produces events, this one consumes them.
// Run them in two terminals against the same handler and the bytes
// emitted by the buyflow run show up here within a few hundred ms.
//
// Run locally:
//
//	# Terminal 1: handler
//	cd ../../../..
//	cargo run --release
//
//	# Terminal 2: listener
//	cd clients/go/stateset-icp-go
//	go run ./examples/listen
//
//	# Terminal 3: producer (or any write against /icp/v1/intents)
//	go run ./examples/buyflow
//
// Ctrl-C exits cleanly: SIGINT cancels the context, which unblocks the
// iterator's Next() and releases the underlying HTTP connection via
// defer stream.Close(). No leaked goroutines or sockets.
package main

import (
	"context"
	"fmt"
	"log"
	"os"
	"os/signal"
	"syscall"
	"time"

	icp "github.com/stateset/stateset-icp-go"
)

// Fixed-width type column so the output tails cleanly in a terminal.
const typeWidth = 28

func env(key, fallback string) string {
	if got := os.Getenv(key); got != "" {
		return got
	}
	return fallback
}

func main() {
	url := env("ICP_URL", "http://localhost:8082")
	apiKey := env("ICP_API_KEY", "icp_demo_key_123")
	agentID := env("ICP_AGENT_ID", "did:stateset:agent:observer-go")

	// signal.NotifyContext cancels `ctx` on SIGINT/SIGTERM so the
	// iterator unblocks cleanly. The stop function releases the
	// signal handler so we don't keep catching signals past this
	// main's lifetime.
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	fmt.Printf("connecting to %s as %s\n", url, agentID)
	c := icp.New(url, apiKey, agentID)

	stream, err := c.Events(ctx)
	if err != nil {
		log.Fatalf("events: %v", err)
	}
	defer stream.Close()

	fmt.Printf("listening on %s/icp/v1/events:stream ... (ctrl-c to exit)\n", url)
	for stream.Next() {
		ev := stream.Event()
		ts := time.Now().Format("2006-01-02T15:04:05")
		name := padRight(nonEmpty(ev.Type, "<untyped>"), typeWidth)
		fmt.Printf("[%s] %s %s\n", ts, name, summary(ev))
	}

	if err := stream.Err(); err != nil {
		log.Fatalf("stream: %v", err)
	}
	// Err() == nil on EOF or cancellation. Distinguish cleanly for
	// the user so they can tell "handler closed the stream" (EOF)
	// from "ctrl-c" (cancel).
	if ctx.Err() != nil {
		fmt.Println("\nstopped.")
		return
	}
	fmt.Println("\nserver closed stream.")
}

// summary picks the most interesting fields from an event payload and
// joins them on spaces. Matches the shape `clients/python/examples/
// listen.py` produces so the two demos tail comparably. Returns "" for
// events with no recognizable fields (keep-alives after future parser
// changes, or opaque peer-quote extensions).
func summary(ev icp.SseEvent) string {
	if ev.Data == nil {
		return ""
	}
	var parts []string
	for _, key := range []string{
		"transaction_id",
		"subscription_id",
		"peer_quote_id",
		"order_id",
		"agent_id",
	} {
		if v, ok := ev.Data[key]; ok {
			if s, ok := v.(string); ok && s != "" {
				parts = append(parts, key+"="+s)
			}
		}
	}
	// Totals (nested) — include when present for transaction events
	// so "transaction.quoted   transaction_id=txn_1  total=USD 59.98"
	// is readable at a glance.
	if totals, ok := ev.Data["totals"].(map[string]any); ok {
		if total, ok := totals["total"].(map[string]any); ok {
			ccy, _ := total["currency"].(string)
			if amt, ok := total["amount_minor"].(float64); ok && ccy != "" {
				parts = append(parts, fmt.Sprintf("total=%s %.2f", ccy, amt/100))
			}
		}
	}
	return join(parts, "  ")
}

// padRight left-aligns s in a field of width n, padding with spaces.
// Shorter than strings.Repeat + len math in the hot path and legible
// without an import.
func padRight(s string, n int) string {
	if len(s) >= n {
		return s
	}
	out := make([]byte, n)
	copy(out, s)
	for i := len(s); i < n; i++ {
		out[i] = ' '
	}
	return string(out)
}

func nonEmpty(s, fallback string) string {
	if s == "" {
		return fallback
	}
	return s
}

func join(parts []string, sep string) string {
	if len(parts) == 0 {
		return ""
	}
	out := parts[0]
	for _, p := range parts[1:] {
		out += sep + p
	}
	return out
}
