package main

// Covers the terminal-output helpers so a refactor of summary() can't
// silently strip fields the user expects to see. Exercises the three
// ICP event families against the same data shapes the handler emits
// (transaction.*, subscription.*, peer_quote.*).

import (
	"strings"
	"testing"

	icp "github.com/stateset/stateset-icp-go"
)

func TestSummaryFormatsTransactionWithTotals(t *testing.T) {
	got := summary(icp.SseEvent{
		Type: "transaction.quoted",
		Data: map[string]any{
			"transaction_id": "txn_1",
			"totals": map[string]any{
				"total": map[string]any{
					"amount_minor": float64(5998),
					"currency":     "USD",
				},
			},
		},
	})
	if !strings.Contains(got, "transaction_id=txn_1") {
		t.Errorf("missing transaction_id: %q", got)
	}
	if !strings.Contains(got, "total=USD 59.98") {
		t.Errorf("total formatting wrong: %q", got)
	}
}

func TestSummarySubscriptionFamily(t *testing.T) {
	got := summary(icp.SseEvent{
		Type: "subscription.renewed",
		Data: map[string]any{
			"subscription_id": "sub_3",
			"agent_id":        "did:stateset:agent:x",
		},
	})
	if !strings.Contains(got, "subscription_id=sub_3") {
		t.Errorf("missing subscription_id: %q", got)
	}
	if !strings.Contains(got, "agent_id=did:stateset:agent:x") {
		t.Errorf("missing agent_id: %q", got)
	}
}

func TestSummaryPeerQuoteFamily(t *testing.T) {
	got := summary(icp.SseEvent{
		Type: "peer_quote.accepted",
		Data: map[string]any{
			"peer_quote_id": "pq_9",
		},
	})
	if got != "peer_quote_id=pq_9" {
		t.Errorf("peer quote summary = %q, want exactly \"peer_quote_id=pq_9\"", got)
	}
}

func TestSummaryEmptyOnKeepAlive(t *testing.T) {
	got := summary(icp.SseEvent{Type: "keep-alive", Data: nil})
	if got != "" {
		t.Errorf("keep-alive should produce empty summary, got %q", got)
	}
}

func TestPadRightPadsShortAndPreservesLong(t *testing.T) {
	if got := padRight("hi", 5); got != "hi   " {
		t.Errorf("padRight(\"hi\", 5) = %q, want \"hi   \"", got)
	}
	if got := padRight("longer", 3); got != "longer" {
		t.Errorf("padRight should not truncate: got %q", got)
	}
}

func TestNonEmptyFallback(t *testing.T) {
	if got := nonEmpty("", "X"); got != "X" {
		t.Errorf("nonEmpty fallback broken")
	}
	if got := nonEmpty("real", "X"); got != "real" {
		t.Errorf("nonEmpty preferred value broken")
	}
}

func TestJoinEmptyVsOne(t *testing.T) {
	if got := join(nil, "-"); got != "" {
		t.Errorf("join(nil) = %q, want empty", got)
	}
	if got := join([]string{"a"}, "-"); got != "a" {
		t.Errorf("join([a]) = %q, want \"a\"", got)
	}
	if got := join([]string{"a", "b", "c"}, "-"); got != "a-b-c" {
		t.Errorf("join = %q", got)
	}
}
