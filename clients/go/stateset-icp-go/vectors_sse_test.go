package icp

// Cross-language SSE-parser interop test. Loads the shared fixture at
// docs/specification/vectors/sse_events.json and asserts that the Go
// text/event-stream parser (in events.go) produces the expected
// SseEvent sequence for each case.
//
// Paired with the equivalent Python test in
// clients/python/tests/test_vectors_sse.py — the two together pin
// the on-wire framing semantics across languages. If one side drifts,
// one side's fixture assertion breaks and we learn about it at
// test-time instead of in production.

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

type sseVectorEvent struct {
	ID   string         `json:"id"`
	Type string         `json:"type"`
	Raw  string         `json:"raw"`
	Data map[string]any `json:"data"`
}

type sseVectorCase struct {
	Name           string           `json:"name"`
	Lines          []string         `json:"lines"`
	ExpectedEvents []sseVectorEvent `json:"expected_events"`
}

type sseVectorFile struct {
	Vectors []sseVectorCase `json:"vectors"`
}

func loadSSEVectors(t *testing.T) sseVectorFile {
	t.Helper()
	path := filepath.Join("..", "..", "..", "docs", "specification", "vectors", "sse_events.json")
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Skipf("sse_events.json not found at %s; skipping cross-language SSE check", path)
	}
	// UseNumber isn't strictly necessary here — expected events have
	// no integer fields today — but we keep the discipline in case a
	// future vector adds one.
	dec := json.NewDecoder(bytes.NewReader(raw))
	var file sseVectorFile
	if err := dec.Decode(&file); err != nil {
		t.Fatalf("parse sse_events.json: %v", err)
	}
	if len(file.Vectors) == 0 {
		t.Fatal("sse_events.json has no vectors")
	}
	return file
}

// runVector drives the EventStream parser end-to-end against a canned
// body synthesized from the vector's `lines` array. Uses the real
// HTTP path (not just the parser directly) so the test covers
// everything from byte-on-the-wire to SseEvent.
func runVector(t *testing.T, c sseVectorCase) []SseEvent {
	t.Helper()
	body := strings.Join(c.Lines, "\n") + "\n"
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		_, _ = io.WriteString(w, body)
	}))
	t.Cleanup(srv.Close)

	client := New(srv.URL, "k", "did:test")
	stream, err := client.Events(context.Background())
	if err != nil {
		t.Fatalf("%s: Events(): %v", c.Name, err)
	}
	t.Cleanup(func() { _ = stream.Close() })

	var events []SseEvent
	for stream.Next() {
		events = append(events, stream.Event())
	}
	if err := stream.Err(); err != nil {
		t.Fatalf("%s: stream.Err: %v", c.Name, err)
	}
	return events
}

func TestVectorsSSE(t *testing.T) {
	file := loadSSEVectors(t)

	for _, vc := range file.Vectors {
		vc := vc // pin for subtest closure
		t.Run(vc.Name, func(t *testing.T) {
			got := runVector(t, vc)
			if len(got) != len(vc.ExpectedEvents) {
				t.Fatalf(
					"event count mismatch: got %d, want %d\n  got:  %+v\n  want: %+v",
					len(got), len(vc.ExpectedEvents), got, vc.ExpectedEvents,
				)
			}
			for i, want := range vc.ExpectedEvents {
				g := got[i]
				if g.ID != want.ID {
					t.Errorf("events[%d].ID = %q, want %q", i, g.ID, want.ID)
				}
				if g.Type != want.Type {
					t.Errorf("events[%d].Type = %q, want %q", i, g.Type, want.Type)
				}
				if g.Raw != want.Raw {
					t.Errorf("events[%d].Raw mismatch\n  got:  %q\n  want: %q", i, g.Raw, want.Raw)
				}
				// Data is a map[string]any on both sides; reflect.DeepEqual
				// handles nested maps and slices. The JSON unmarshaler on
				// the test file produces the same shapes the handler
				// emits, so a direct comparison is sound.
				if !reflect.DeepEqual(g.Data, want.Data) {
					t.Errorf(
						"events[%d].Data mismatch\n  got:  %#v\n  want: %#v",
						i, g.Data, want.Data,
					)
				}
			}
		})
	}
}
