package icp

// Ed25519 keypair + `did:key` construction + compact-JWS mandate
// signing (ICP_SPEC §6).
//
// Stdlib-only: crypto/ed25519, crypto/rand, encoding/base64,
// encoding/json, math/big. Matches the Rust reference and the Python
// client byte-for-byte given the same inputs (see
// docs/specification/vectors/ for the shared fixtures).

import (
	"crypto/ed25519"
	"crypto/rand"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"math/big"
	"time"
)

// ---- base58btc -------------------------------------------------------

// Bitcoin-style base58 alphabet. Used under the multibase `z` prefix
// for did:key (W3C did:key spec).
var base58Alphabet = []byte("123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz")

// base58btcEncode converts raw bytes to base58btc. Hand-rolled because
// pulling a base58 dep for one call is overkill and we're committed
// to stdlib-only.
func base58btcEncode(data []byte) string {
	// Count leading zero bytes — each maps to one leading '1'
	// character (per Bitcoin base58 convention).
	leading := 0
	for _, b := range data {
		if b != 0 {
			break
		}
		leading++
	}
	// Treat the rest as a big-endian integer and convert to base-58.
	num := new(big.Int).SetBytes(data)
	fiftyEight := big.NewInt(58)
	zero := big.NewInt(0)
	mod := new(big.Int)

	var out []byte
	for num.Cmp(zero) > 0 {
		num.DivMod(num, fiftyEight, mod)
		out = append(out, base58Alphabet[mod.Int64()])
	}
	for i := 0; i < leading; i++ {
		out = append(out, base58Alphabet[0])
	}
	// We built the digits least-significant-first; reverse for
	// big-endian base58.
	for i, j := 0, len(out)-1; i < j; i, j = i+1, j-1 {
		out[i], out[j] = out[j], out[i]
	}
	return string(out)
}

// ---- did:key ---------------------------------------------------------

// DIDKeyFromPublicKey encodes an Ed25519 public key as a `did:key`
// identifier. Follows W3C did:key for Ed25519: multicodec prefix
// `0xed 0x01` + 32-byte raw pubkey, base58btc-encoded with the
// multibase `z` prefix.
func DIDKeyFromPublicKey(pk ed25519.PublicKey) (string, error) {
	if len(pk) != ed25519.PublicKeySize {
		return "", fmt.Errorf("ed25519 public key must be %d bytes, got %d",
			ed25519.PublicKeySize, len(pk))
	}
	prefixed := make([]byte, 0, 2+ed25519.PublicKeySize)
	prefixed = append(prefixed, 0xed, 0x01)
	prefixed = append(prefixed, pk...)
	return "did:key:z" + base58btcEncode(prefixed), nil
}

// ---- keypair ---------------------------------------------------------

// Ed25519KeyPair is a sign/verify pair plus its `did:key` identifier.
// The kid used in signed JWS headers defaults to the full DID, so a
// verifying handler can look up the key by kid alone.
type Ed25519KeyPair struct {
	Private ed25519.PrivateKey
	Public  ed25519.PublicKey
	DID     string
	Kid     string
}

// GenerateKeyPair mints a fresh Ed25519 keypair.
func GenerateKeyPair() (*Ed25519KeyPair, error) {
	pub, priv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		return nil, err
	}
	return wrapKey(priv, pub)
}

// KeyPairFromSeed reconstructs a keypair from a deterministic 32-byte
// Ed25519 seed. Useful for reproducing known-answer tests (including
// the shared golden vectors).
func KeyPairFromSeed(seed []byte) (*Ed25519KeyPair, error) {
	if len(seed) != ed25519.SeedSize {
		return nil, fmt.Errorf("ed25519 seed must be %d bytes, got %d",
			ed25519.SeedSize, len(seed))
	}
	priv := ed25519.NewKeyFromSeed(seed)
	pub := priv.Public().(ed25519.PublicKey)
	return wrapKey(priv, pub)
}

func wrapKey(priv ed25519.PrivateKey, pub ed25519.PublicKey) (*Ed25519KeyPair, error) {
	did, err := DIDKeyFromPublicKey(pub)
	if err != nil {
		return nil, err
	}
	return &Ed25519KeyPair{Private: priv, Public: pub, DID: did, Kid: did}, nil
}

// ---- mandate payload helper -----------------------------------------

// MandateOpts packages the fields every reasonable mandate carries.
// Zero-valued fields are filled with sensible defaults: scope and
// budget are required, everything else takes a sane default.
type MandateOpts struct {
	Issuer               string
	Subject              string
	Scope                []string
	BudgetCurrency       string
	BudgetAmountMinor    int64
	BudgetPerTransaction *int64 // nil → null on the wire
	BudgetPeriod         string // defaults to "P1D"
	Merchants            []string
	Categories           []string
	Jurisdictions        []string
	RequireReceipt       bool
	ValidForSecs         int64 // defaults to 3600
	JTI                  string
	NowSecs              int64 // 0 → time.Now().Unix()
	Version              string // defaults to "2026-04-21"
}

// NewMandatePayload builds a MandatePayload dict matching ICP_SPEC §6.
// Returns a map[string]any that SignMandate consumes. All integer
// values use int64 so JSON marshaling emits plain decimals (not
// exponential — Go's default float64 formatter would emit "1.745e+09"
// for our iat timestamps, which is why int64 matters).
func NewMandatePayload(o MandateOpts) (map[string]any, error) {
	if o.Issuer == "" || o.Subject == "" {
		return nil, errors.New("icp: MandateOpts requires Issuer and Subject")
	}
	if len(o.Scope) == 0 {
		return nil, errors.New("icp: MandateOpts requires at least one Scope entry")
	}
	if o.BudgetCurrency == "" {
		return nil, errors.New("icp: MandateOpts requires BudgetCurrency")
	}
	now := o.NowSecs
	if now == 0 {
		now = time.Now().Unix()
	}
	valid := o.ValidForSecs
	if valid == 0 {
		valid = 3600
	}
	period := o.BudgetPeriod
	if period == "" {
		period = "P1D"
	}
	version := o.Version
	if version == "" {
		version = "2026-04-21"
	}
	jti := o.JTI
	if jti == "" {
		jti = "mandate-" + randomHex(16)
	}
	merchants := o.Merchants
	if merchants == nil {
		merchants = []string{"*"}
	}
	// Defaulting nil → [] so the wire shape is stable. An omitted
	// `categories` field would deserialize into `nil` on both sides
	// via `#[serde(default)]`, but shipping the empty array keeps
	// the JCS output identical to the Python client.
	categories := o.Categories
	if categories == nil {
		categories = []string{}
	}
	jurisdictions := o.Jurisdictions
	if jurisdictions == nil {
		jurisdictions = []string{}
	}

	budget := map[string]any{
		"currency":     o.BudgetCurrency,
		"amount_minor": o.BudgetAmountMinor,
		// nil here serializes to `null` — matches Rust `Option::None`
		// and Python `per_transaction: None`. Do not omit the key;
		// the JCS bytes diverge if we do.
		"per_transaction": valueOrNil(o.BudgetPerTransaction),
		"period":          period,
	}

	return map[string]any{
		"iss": o.Issuer,
		"sub": o.Subject,
		"iat": now,
		"nbf": now,
		"exp": now + valid,
		"jti": jti,
		"icp": map[string]any{
			"version":       version,
			"scope":         o.Scope,
			"budget":        budget,
			"merchants":     merchants,
			"categories":    categories,
			"jurisdictions": jurisdictions,
			"policies": map[string]any{
				"require_receipt":                      o.RequireReceipt,
				"require_shipping_address_confirmation": false,
				"prohibit_subscriptions":                false,
			},
			"linked_payment_methods": []string{},
		},
	}, nil
}

func valueOrNil(p *int64) any {
	if p == nil {
		return nil
	}
	return *p
}

func randomHex(n int) string {
	b := make([]byte, n)
	if _, err := rand.Read(b); err != nil {
		// Extremely rare — crypto/rand failure on a running process
		// is a signal the environment is unhealthy. Panicking here
		// is aggressive but correct; the alternative is returning a
		// low-entropy jti that could collide.
		panic("crypto/rand failed: " + err.Error())
	}
	out := make([]byte, n*2)
	const hex = "0123456789abcdef"
	for i, v := range b {
		out[i*2] = hex[v>>4]
		out[i*2+1] = hex[v&0x0f]
	}
	return string(out)
}

// ---- signing --------------------------------------------------------

// SignMandate produces the compact-JWS mandate string
// `base64url(header) + "." + base64url(payload) + "." + base64url(sig)`.
//
// The caller is responsible for constructing `payload` with integer
// fields as `int64` (not `float64`). If the payload was loaded from
// JSON, use `json.Decoder.UseNumber()` to preserve integer encoding —
// see the golden-vector test in `vectors_test.go` for the pattern.
//
// Key ordering is deterministic because Go's `encoding/json` sorts
// map keys by codepoint, matching the JCS rules used by the Rust
// reference (`serde_jcs`) and the Python client
// (`json.dumps(sort_keys=True)`).
func SignMandate(payload map[string]any, kp *Ed25519KeyPair) (string, error) {
	if kp == nil {
		return "", errors.New("icp: SignMandate requires a non-nil keypair")
	}
	header := map[string]any{
		"alg": "EdDSA",
		"typ": "JWT",
		"kid": kp.Kid,
	}
	headerBytes, err := canonicalJSON(header)
	if err != nil {
		return "", fmt.Errorf("serialize header: %w", err)
	}
	payloadBytes, err := canonicalJSON(payload)
	if err != nil {
		return "", fmt.Errorf("serialize payload: %w", err)
	}
	headerB64 := b64url(headerBytes)
	payloadB64 := b64url(payloadBytes)
	signingInput := headerB64 + "." + payloadB64
	sig := ed25519.Sign(kp.Private, []byte(signingInput))
	return signingInput + "." + b64url(sig), nil
}

// canonicalJSON emits a JSON encoding compatible with JCS (RFC 8785)
// for the subset of values ICP mandates use: strings, booleans, null,
// integers, arrays, maps. Key sort order is lexicographic by
// codepoint (what Go's json does by default) matching JCS's
// UTF-16-code-unit sort for all-ASCII keys (every ICP field name).
//
// HTML-safe escaping is disabled so `<`, `>`, `&` survive as-is —
// mandate payloads don't contain them, but disabling the default
// avoids a drift from the Rust reference should any future extension
// ever include HTML-ish strings.
func canonicalJSON(v any) ([]byte, error) {
	var buf errBuf
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	if err := enc.Encode(v); err != nil {
		return nil, err
	}
	out := buf.Bytes()
	// json.Encoder appends a trailing newline; strip it — JCS output
	// has no whitespace.
	if len(out) > 0 && out[len(out)-1] == '\n' {
		out = out[:len(out)-1]
	}
	return out, nil
}

// errBuf is a tiny io.Writer wrapper for json.Encoder that avoids an
// extra bytes.Buffer dependency. Functionally equivalent.
type errBuf struct{ b []byte }

func (e *errBuf) Write(p []byte) (int, error) {
	e.b = append(e.b, p...)
	return len(p), nil
}

func (e *errBuf) Bytes() []byte { return e.b }

func b64url(data []byte) string {
	return base64.RawURLEncoding.EncodeToString(data)
}

