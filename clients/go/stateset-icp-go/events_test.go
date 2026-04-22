package icp

// Tests for the SSE event-stream wrapper. Mirrors the Python
// `test_sse.py` structure: low-level parser checks using in-memory
// scan of a canned body, plus end-to-end tests driving a real
// httptest server that streams text/event-stream.

import (
	"context"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

// sseBody assembles a canonical text/event-stream body from a list of
// SSE-framed lines. Trailing \n matters for the final blank-line
// dispatch — the scanner's ScanLines doesn't emit a trailing empty
// token on EOF otherwise.
func sseBody(lines []string) string {
	return strings.Join(lines, "\n") + "\n"
}

// streamingHandler serves the given body as text/event-stream with
// explicit flushes between chunks. Tests that don't care about
// flush semantics can just write the whole body at once — scanner
// reads until EOF either way.
func streamingHandler(body string) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(200)
		_, _ = io.WriteString(w, body)
	}
}

// ---- Low-level parser (via in-package bufio.Scanner driving) ----

// parseAll drives the EventStream parser against a canned body and
// collects every dispatched event. Avoids the httptest overhead for
// tests that only care about parser semantics.
func parseAll(t *testing.T, body string) []SseEvent {
	t.Helper()
	srv := httptest.NewServer(streamingHandler(body))
	t.Cleanup(srv.Close)
	c := New(srv.URL, "k", "did:test")
	stream, err := c.Events(context.Background())
	if err != nil {
		t.Fatalf("Events(): %v", err)
	}
	t.Cleanup(func() { _ = stream.Close() })

	var out []SseEvent
	for stream.Next() {
		out = append(out, stream.Event())
	}
	if err := stream.Err(); err != nil {
		t.Fatalf("stream.Err: %v", err)
	}
	return out
}

func TestParserDispatchesOnBlankLine(t *testing.T) {
	events := parseAll(t, sseBody([]string{
		"id: 1",
		"event: transaction.quoted",
		`data: {"transaction_id":"txn_7"}`,
		"",
		"id: 2",
		"event: subscription.renewed",
		`data: {"subscription_id":"sub_3"}`,
		"",
	}))
	if len(events) != 2 {
		t.Fatalf("events = %d, want 2", len(events))
	}
	if events[0].ID != "1" || events[0].Type != "transaction.quoted" {
		t.Errorf("event 0 headers wrong: %+v", events[0])
	}
	if txnID, _ := events[0].Data["transaction_id"].(string); txnID != "txn_7" {
		t.Errorf("event 0 data = %+v", events[0].Data)
	}
	if events[1].Type != "subscription.renewed" {
		t.Errorf("event 1 type = %q", events[1].Type)
	}
}

func TestParserConcatenatesMultilineData(t *testing.T) {
	events := parseAll(t, sseBody([]string{
		"event: bulk.message",
		"data: first line",
		"data: second line",
		"",
	}))
	if len(events) != 1 {
		t.Fatalf("events = %d, want 1", len(events))
	}
	if events[0].Raw != "first line\nsecond line" {
		t.Errorf("Raw = %q", events[0].Raw)
	}
	// Multi-line text isn't JSON → Data is nil, Raw preserves everything.
	if events[0].Data != nil {
		t.Errorf("Data should be nil for non-JSON body, got %+v", events[0].Data)
	}
}

func TestParserDiscardsKeepaliveComments(t *testing.T) {
	events := parseAll(t, sseBody([]string{
		": keep-alive",
		":",
		"event: ping",
		"data: pong",
		"",
	}))
	if len(events) != 1 {
		t.Fatalf("events = %d, want 1", len(events))
	}
	if events[0].Type != "ping" || events[0].Raw != "pong" {
		t.Errorf("event wrong: %+v", events[0])
	}
}

func TestParserStripsSingleLeadingSpacePerSpec(t *testing.T) {
	// Per HTML Living Standard §9.2.6: a single leading space after
	// the colon is stripped; additional spaces are preserved.
	events := parseAll(t, sseBody([]string{
		"data:  two-space-preserved",
		"",
	}))
	if len(events) != 1 {
		t.Fatalf("events = %d", len(events))
	}
	if events[0].Raw != " two-space-preserved" {
		t.Errorf("Raw = %q, want leading single-space preserved", events[0].Raw)
	}
}

func TestParserIgnoresUnknownFields(t *testing.T) {
	events := parseAll(t, sseBody([]string{
		"retry: 5000",
		"event: foo",
		"unknown: whatever",
		"data: ok",
		"",
	}))
	if len(events) != 1 {
		t.Fatalf("events = %d", len(events))
	}
	if events[0].Type != "foo" || events[0].Raw != "ok" {
		t.Errorf("event wrong: %+v", events[0])
	}
}

func TestParserNonJSONDataStaysRaw(t *testing.T) {
	events := parseAll(t, sseBody([]string{
		"event: plain",
		"data: not json at all",
		"",
	}))
	if events[0].Data != nil {
		t.Errorf("Data should be nil for non-JSON, got %+v", events[0].Data)
	}
	if events[0].Raw != "not json at all" {
		t.Errorf("Raw = %q", events[0].Raw)
	}
}

// ---- End-to-end via httptest ----------------------------------------

func TestEventsYieldsParsedEventsEndToEnd(t *testing.T) {
	body := sseBody([]string{
		"id: evt_1",
		"event: transaction.quoted",
		`data: {"transaction_id":"txn_1","state":"quoted"}`,
		"",
		"id: evt_2",
		"event: transaction.completed",
		`data: {"transaction_id":"txn_1","state":"completed"}`,
		"",
	})

	var seenReq *http.Request
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		seenReq = r
		w.Header().Set("Content-Type", "text/event-stream")
		_, _ = io.WriteString(w, body)
	}))
	defer srv.Close()

	c := New(srv.URL, "test-key", "did:stateset:agent:go-test")
	stream, err := c.Events(context.Background())
	if err != nil {
		t.Fatalf("Events: %v", err)
	}
	defer stream.Close()

	var seen []SseEvent
	for stream.Next() {
		seen = append(seen, stream.Event())
	}
	if err := stream.Err(); err != nil {
		t.Fatalf("stream.Err: %v", err)
	}

	if len(seen) != 2 {
		t.Fatalf("got %d events, want 2", len(seen))
	}
	wantTypes := []string{"transaction.quoted", "transaction.completed"}
	for i, want := range wantTypes {
		if seen[i].Type != want {
			t.Errorf("seen[%d].Type = %q, want %q", i, seen[i].Type, want)
		}
	}
	if state, _ := seen[0].Data["state"].(string); state != "quoted" {
		t.Errorf("seen[0].Data.state = %q", state)
	}

	// Request-side sanity.
	if seenReq.URL.Path != "/icp/v1/events:stream" {
		t.Errorf("path = %q", seenReq.URL.Path)
	}
	if got := seenReq.Header.Get("Accept"); got != "text/event-stream" {
		t.Errorf("Accept = %q", got)
	}
	if got := seenReq.Header.Get("Authorization"); !strings.HasPrefix(got, "Bearer ") {
		t.Errorf("missing bearer auth")
	}
}

func TestEventsReturnsIcpErrorOn401(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(401)
		_, _ = io.WriteString(w, `{"error":{"type":"auth","code":"auth_failed","message":"nope"}}`)
	}))
	defer srv.Close()

	c := New(srv.URL, "wrong-key", "did:x")
	_, err := c.Events(context.Background())
	if err == nil {
		t.Fatal("expected IcpError")
	}
	icpErr, ok := err.(*IcpError)
	if !ok {
		t.Fatalf("expected *IcpError, got %T: %v", err, err)
	}
	if icpErr.StatusCode != 401 || icpErr.Code != "auth_failed" {
		t.Errorf("IcpError = %+v", icpErr)
	}
}

func TestEventsContextCancellationUnblocksNext(t *testing.T) {
	// Server that dribbles events and stays open until the client
	// gives up. Verifies that canceling the context on the client
	// side reliably unblocks Next() on an idle stream — without
	// this, a forgotten cancel would leak a goroutine blocked in
	// bufio.Scanner.Scan().
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		flusher, _ := w.(http.Flusher)
		_, _ = io.WriteString(w, "event: first\ndata: {}\n\n")
		if flusher != nil {
			flusher.Flush()
		}
		<-r.Context().Done() // stay open until client disconnects
	}))
	defer srv.Close()

	ctx, cancel := context.WithCancel(context.Background())
	c := New(srv.URL, "k", "did:x")
	stream, err := c.Events(ctx)
	if err != nil {
		t.Fatalf("Events: %v", err)
	}
	defer stream.Close()

	// Drain the first event so we're sitting in Next() waiting
	// for more data when the cancel fires.
	if !stream.Next() {
		t.Fatalf("expected first event, got Err: %v", stream.Err())
	}
	if stream.Event().Type != "first" {
		t.Errorf("first event = %+v", stream.Event())
	}

	// Cancel, then confirm Next() returns false promptly.
	cancel()
	done := make(chan bool, 1)
	go func() { done <- stream.Next() }()
	select {
	case got := <-done:
		if got {
			t.Error("Next() returned true after cancel")
		}
	case <-time.After(2 * time.Second):
		t.Error("Next() did not unblock after context cancel")
	}
}

func TestCloseIsIdempotent(t *testing.T) {
	srv := httptest.NewServer(streamingHandler(sseBody([]string{
		"event: one", "data: {}", "",
	})))
	defer srv.Close()

	c := New(srv.URL, "k", "did:x")
	stream, err := c.Events(context.Background())
	if err != nil {
		t.Fatalf("Events: %v", err)
	}
	if err := stream.Close(); err != nil {
		t.Errorf("first Close: %v", err)
	}
	if err := stream.Close(); err != nil {
		t.Errorf("second Close: %v", err)
	}
}

