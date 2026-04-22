package icp

// Server-Sent Events (SSE) iteration for GET /icp/v1/events:stream.
//
// Follows the HTML Living Standard §9.2.6 parsing rules: blank line
// dispatches the accumulated event; `:` lines are comments (typically
// keep-alives); multiple `data:` lines concatenate with `\n`; a single
// leading space after the colon is stripped; unknown fields are
// silently ignored.
//
// Go idiom: iterator pattern (Next / Event / Err / Close), matching
// sql.Rows and bufio.Scanner. Always close via defer to free the
// underlying HTTP connection.

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
)

// SseEvent is a single event parsed from /icp/v1/events:stream.
//
// Data is auto-populated when the body is a valid JSON object; Raw
// always holds the exact `data:` body as received, so callers can
// re-parse it differently if they need to.
type SseEvent struct {
	ID   string
	Type string
	Data map[string]any
	Raw  string
}

// EventStream iterates server-sent events. Created by Client.Events.
// Typical use:
//
//	stream, err := c.Events(ctx)
//	if err != nil { … }
//	defer stream.Close()
//	for stream.Next() {
//	    ev := stream.Event()
//	    if ev.Type == "transaction.completed" { … }
//	}
//	if err := stream.Err(); err != nil { … }
type EventStream struct {
	resp    *http.Response
	scanner *bufio.Scanner
	cur     SseEvent
	err     error
	closed  bool

	// Accumulating state between Next() calls. SSE allows an event
	// to span multiple lines (id + type + multi-line data + blank).
	id, typ string
	data    []string
}

// Events opens a streaming connection to /icp/v1/events:stream.
// Returns a non-nil *EventStream on success that the caller MUST
// close via stream.Close().
//
// Pass a context to control stream lifetime — canceling the context
// via ctx cancellation will unblock Next() and return false; use
// stream.Err() to distinguish cancellation from EOF.
func (c *Client) Events(ctx context.Context) (*EventStream, error) {
	req, err := http.NewRequestWithContext(ctx, "GET", c.baseURL+"/icp/v1/events:stream", nil)
	if err != nil {
		return nil, err
	}
	c.applyAuthHeaders(req)
	req.Header.Set("Accept", "text/event-stream")
	req.Header.Set("ICP-Request-Id", autoRequestID())

	resp, err := c.http.Do(req)
	if err != nil {
		return nil, err
	}
	if resp.StatusCode >= 400 {
		// Surface 4xx/5xx as IcpError for parity with the rest of
		// the client surface. Drain + close the body first.
		body, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		return nil, parseError(resp.StatusCode, body)
	}

	sc := bufio.NewScanner(resp.Body)
	// Default ScanLines handles both \n and \r\n. Generous buffer
	// so a single big JSON event doesn't overflow.
	sc.Buffer(make([]byte, 64*1024), 1024*1024)
	return &EventStream{resp: resp, scanner: sc}, nil
}

// Next advances to the next event. Returns false on stream close,
// cancellation, or error. Always followed by Event() (to read the
// just-dispatched event) or Err() (to check why iteration stopped).
func (s *EventStream) Next() bool {
	if s.closed {
		return false
	}
	for s.scanner.Scan() {
		line := s.scanner.Text()

		if line == "" {
			// Blank line dispatches an event *iff* we've
			// accumulated anything. An event with only an id or
			// only a type is still a valid dispatch — matches
			// the Python parser.
			if s.id != "" || s.typ != "" || len(s.data) > 0 {
				raw := strings.Join(s.data, "\n")
				var data map[string]any
				if raw != "" {
					// Silent fall-through to nil on parse error — caller
					// can still read ev.Raw if they need a non-JSON payload.
					_ = json.Unmarshal([]byte(raw), &data)
				}
				s.cur = SseEvent{
					ID:   s.id,
					Type: s.typ,
					Data: data,
					Raw:  raw,
				}
				s.id, s.typ, s.data = "", "", nil
				return true
			}
			continue
		}

		if strings.HasPrefix(line, ":") {
			// Keep-alive / comment. Discard per spec.
			continue
		}

		// `field: value` or `field` (empty value).
		idx := strings.IndexByte(line, ':')
		var field, value string
		if idx < 0 {
			field, value = line, ""
		} else {
			field = line[:idx]
			value = line[idx+1:]
		}
		// Spec: a single leading space on the value is stripped;
		// additional whitespace is preserved as-is.
		if strings.HasPrefix(value, " ") {
			value = value[1:]
		}
		switch field {
		case "id":
			s.id = value
		case "event":
			s.typ = value
		case "data":
			s.data = append(s.data, value)
		default:
			// `retry:` and any other unknown field: ignore per spec.
		}
	}

	// Scanner stopped. Distinguish EOF from error.
	if err := s.scanner.Err(); err != nil {
		s.err = fmt.Errorf("read event stream: %w", err)
	}
	return false
}

// Event returns the event produced by the most recent successful
// Next() call. Undefined before the first Next() returns true.
func (s *EventStream) Event() SseEvent { return s.cur }

// Err returns the first error encountered during iteration, if any.
// nil means iteration stopped cleanly (EOF or Close).
func (s *EventStream) Err() error { return s.err }

// Close releases the underlying HTTP connection. Safe to call
// multiple times.
func (s *EventStream) Close() error {
	if s.closed {
		return nil
	}
	s.closed = true
	if s.resp != nil {
		return s.resp.Body.Close()
	}
	return nil
}
