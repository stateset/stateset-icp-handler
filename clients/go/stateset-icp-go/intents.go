package icp

import "errors"

// Ergonomic wrappers for every intent in the ICP-Full catalog (spec
// §7.2). Each is a thin layer over SubmitIntent that fixes the wire
// intent name, passes the caller's params through, and returns the
// handler response. Complex intents take a typed *Params struct;
// simpler ones take positional args.
//
// Keeping these in a separate file from client.go so the transport
// layer stays small and auditable, and so growth in the intent
// catalog doesn't bloat the file that holds the HTTP machinery.

// --- Read-side -------------------------------------------------------

// Search queries the merchant's catalog (ICP-Full: intent.search).
// Read-only; typically does not require a mandate.
func (c *Client) Search(query string, limit int, opts SubmitOptions) (map[string]any, error) {
	params := map[string]any{}
	if query != "" {
		params["query"] = query
	}
	if limit > 0 {
		params["limit"] = limit
	}
	return c.call("intent.search", params, opts)
}

// Describe fetches product detail by product ID or SKU (exactly one;
// the handler rejects requests with both or neither).
func (c *Client) Describe(productID, sku string, opts SubmitOptions) (map[string]any, error) {
	if (productID == "") == (sku == "") {
		return nil, errors.New("icp: Describe requires exactly one of productID or sku")
	}
	params := map[string]any{}
	if productID != "" {
		params["product_id"] = productID
	}
	if sku != "" {
		params["sku"] = sku
	}
	return c.call("intent.describe", params, opts)
}

// Track returns shipment + fulfillment status for a transaction.
func (c *Client) Track(transactionID string, opts SubmitOptions) (map[string]any, error) {
	return c.call("intent.track", map[string]any{"transaction_id": transactionID}, opts)
}

// --- Buy lifecycle ---------------------------------------------------

// QuoteParams drives intent.quote. Items is required; everything else
// is optional and omitted from the envelope when zero-valued.
type QuoteParams struct {
	Items        []map[string]any
	Buyer        map[string]any
	ShipTo       map[string]any
	Currency     string // defaults to "USD" on the handler
	Jurisdiction string
}

// Quote requests a priced quote for the given basket.
func (c *Client) Quote(p QuoteParams, opts SubmitOptions) (map[string]any, error) {
	params := map[string]any{"items": p.Items}
	if p.Buyer != nil {
		params["buyer"] = p.Buyer
	}
	if p.ShipTo != nil {
		params["ship_to"] = p.ShipTo
	}
	ctx := map[string]any{}
	if p.Currency != "" {
		ctx["currency"] = p.Currency
	}
	if p.Jurisdiction != "" {
		ctx["jurisdiction"] = p.Jurisdiction
	}
	env := IntentEnvelope{
		Intent:  "intent.quote",
		AgentID: c.agentID,
		Params:  params,
	}
	if len(ctx) > 0 {
		env.Context = ctx
	}
	return c.SubmitIntent(env, opts)
}

// Authorize authorizes a previously-quoted transaction.
func (c *Client) Authorize(transactionID string, opts SubmitOptions) (map[string]any, error) {
	return c.call("intent.authorize", map[string]any{"transaction_id": transactionID}, opts)
}

// Buy captures payment and places the order. Call after Authorize.
func (c *Client) Buy(transactionID string, payment map[string]any, opts SubmitOptions) (map[string]any, error) {
	return c.call("intent.buy", map[string]any{
		"transaction_id": transactionID,
		"payment":        payment,
	}, opts)
}

// Pay is the one-shot path: capture without a preceding Authorize.
func (c *Client) Pay(transactionID string, payment map[string]any, opts SubmitOptions) (map[string]any, error) {
	return c.call("intent.pay", map[string]any{
		"transaction_id": transactionID,
		"payment":        payment,
	}, opts)
}

// --- Post-sale -------------------------------------------------------

// Return initiates a return. Named `Return` rather than some
// workaround because Go's `return` keyword is lower-case only —
// exported identifiers aren't reserved.
func (c *Client) Return(transactionID string, items []map[string]any, reason string, opts SubmitOptions) (map[string]any, error) {
	params := map[string]any{
		"transaction_id": transactionID,
		"items":          items,
	}
	if reason != "" {
		params["reason"] = reason
	}
	return c.call("intent.return", params, opts)
}

// RefundRequest requests a refund. Pass `amount = nil` for a full
// refund; pass a Money map (`{amount_minor, currency}`) for partial.
func (c *Client) RefundRequest(transactionID string, amount map[string]any, reason string, opts SubmitOptions) (map[string]any, error) {
	params := map[string]any{"transaction_id": transactionID}
	if amount != nil {
		params["amount"] = amount
	}
	if reason != "" {
		params["reason"] = reason
	}
	return c.call("intent.refund_request", params, opts)
}

// ConfirmReceipt is the buyer's acknowledgement of physical receipt —
// the escrow-release trigger on A2A and stablecoin flows.
func (c *Client) ConfirmReceipt(transactionID, note string, opts SubmitOptions) (map[string]any, error) {
	params := map[string]any{"transaction_id": transactionID}
	if note != "" {
		params["note"] = note
	}
	return c.call("intent.confirm_receipt", params, opts)
}

// NegotiateParams drives intent.negotiate. Exactly one of
// ProposedTotal or DiscountPct must be non-zero; the wrapper rejects
// ambiguous or empty forms client-side so a live handler doesn't
// have to adjudicate.
type NegotiateParams struct {
	TransactionID string
	// Whole-basket override, formatted as a Money map:
	// {"amount_minor": int, "currency": string}.
	ProposedTotal map[string]any
	// Whole-basket discount as a float in [0.0, 90.0].
	DiscountPct float64
	Message     string
}

// Negotiate counter-offers the totals on a quoted transaction.
func (c *Client) Negotiate(p NegotiateParams, opts SubmitOptions) (map[string]any, error) {
	hasProposed := p.ProposedTotal != nil
	hasDiscount := p.DiscountPct != 0
	if hasProposed == hasDiscount {
		return nil, errors.New("icp: Negotiate requires exactly one of ProposedTotal or DiscountPct")
	}
	params := map[string]any{"transaction_id": p.TransactionID}
	if hasProposed {
		params["proposed_total"] = p.ProposedTotal
	}
	if hasDiscount {
		params["discount_pct"] = p.DiscountPct
	}
	if p.Message != "" {
		params["message"] = p.Message
	}
	return c.call("intent.negotiate", params, opts)
}

// --- Subscriptions ---------------------------------------------------

// SubscribeParams drives intent.subscribe.
type SubscribeParams struct {
	Items    []map[string]any
	Cadence  string // "weekly" | "monthly" | "annual"
	Buyer    map[string]any
	ShipTo   map[string]any
	Payment  map[string]any
	Currency string // defaults to "USD"
}

// Subscribe starts a recurring subscription.
func (c *Client) Subscribe(p SubscribeParams, opts SubmitOptions) (map[string]any, error) {
	params := map[string]any{
		"items":   p.Items,
		"cadence": p.Cadence,
	}
	if p.Currency != "" {
		params["currency"] = p.Currency
	}
	if p.Buyer != nil {
		params["buyer"] = p.Buyer
	}
	if p.ShipTo != nil {
		params["ship_to"] = p.ShipTo
	}
	if p.Payment != nil {
		params["payment"] = p.Payment
	}
	return c.call("intent.subscribe", params, opts)
}

// Renew forces a renewal charge now and resets the dunning failure counter.
func (c *Client) Renew(subscriptionID string, opts SubmitOptions) (map[string]any, error) {
	return c.call("intent.renew", map[string]any{"subscription_id": subscriptionID}, opts)
}

// Pause pauses auto-billing. Returns to active via Renew.
func (c *Client) Pause(subscriptionID string, opts SubmitOptions) (map[string]any, error) {
	return c.call("intent.pause", map[string]any{"subscription_id": subscriptionID}, opts)
}

// CancelSubscription is terminal — no further charges, no reactivation.
func (c *Client) CancelSubscription(subscriptionID string, opts SubmitOptions) (map[string]any, error) {
	return c.call("intent.cancel_subscription", map[string]any{"subscription_id": subscriptionID}, opts)
}

// --- Agent-to-agent (A2A) -------------------------------------------

// A2AQuoteParams drives intent.a2a_quote.
type A2AQuoteParams struct {
	PeerAgentID string
	// Service kind + free-form params — handler-typed enum lives
	// at `service.kind` ∈ {compute, data_feed, image_generation,
	// ad_hoc}.
	Service       map[string]any
	PriceHint     map[string]any // Money map
	ExpiresInSecs int
	ReferenceID   string
}

// A2AQuote asks a peer agent for a quote on a service.
func (c *Client) A2AQuote(p A2AQuoteParams, opts SubmitOptions) (map[string]any, error) {
	params := map[string]any{
		"peer_agent_id": p.PeerAgentID,
		"service":       p.Service,
	}
	if p.PriceHint != nil {
		params["price_hint"] = p.PriceHint
	}
	if p.ExpiresInSecs > 0 {
		// Wire field is `expires_in_seconds` (src/models.rs); the old
		// `expires_in_secs` key was silently ignored, defaulting to 300s.
		params["expires_in_seconds"] = p.ExpiresInSecs
	}
	if p.ReferenceID != "" {
		params["reference_id"] = p.ReferenceID
	}
	return c.call("intent.a2a_quote", params, opts)
}

// A2APayParams drives intent.a2a_pay. Two mutually-exclusive shapes:
//
//   - Pay-against-quote: set PeerQuoteID only. PeerAgentID and
//     Amount must be empty.
//   - Direct-pay: set PeerAgentID + Amount. PeerQuoteID must be
//     empty.
//
// The wrapper enforces both rules client-side.
type A2APayParams struct {
	FromWallet  string
	PeerQuoteID string
	PeerAgentID string
	Amount      map[string]any // Money map
	Memo        string
}

// A2APay pays a peer. See A2APayParams for the two supported shapes.
func (c *Client) A2APay(p A2APayParams, opts SubmitOptions) (map[string]any, error) {
	if p.FromWallet == "" {
		return nil, errors.New("icp: A2APay requires FromWallet")
	}
	againstQuote := p.PeerQuoteID != ""
	direct := p.PeerAgentID != "" || p.Amount != nil
	switch {
	case againstQuote && direct:
		return nil, errors.New("icp: A2APay: set either PeerQuoteID or PeerAgentID+Amount, not both")
	case !againstQuote && !direct:
		return nil, errors.New("icp: A2APay: set PeerQuoteID (pay-against-quote) or PeerAgentID+Amount (direct-pay)")
	case direct && (p.PeerAgentID == "" || p.Amount == nil):
		return nil, errors.New("icp: A2APay direct-pay requires both PeerAgentID and Amount")
	}
	params := map[string]any{"from": p.FromWallet}
	if p.PeerQuoteID != "" {
		params["peer_quote_id"] = p.PeerQuoteID
	}
	if p.PeerAgentID != "" {
		params["peer_agent_id"] = p.PeerAgentID
	}
	if p.Amount != nil {
		params["amount"] = p.Amount
	}
	if p.Memo != "" {
		params["memo"] = p.Memo
	}
	return c.call("intent.a2a_pay", params, opts)
}

// --- Shared envelope builder ----------------------------------------

// call wraps the common case: intent name + params + standard
// IntentEnvelope. Any intent that needs a top-level `context` goes
// through SubmitIntent directly (e.g. Quote).
func (c *Client) call(intent string, params map[string]any, opts SubmitOptions) (map[string]any, error) {
	return c.SubmitIntent(IntentEnvelope{
		Intent:  intent,
		AgentID: c.agentID,
		Params:  params,
	}, opts)
}
