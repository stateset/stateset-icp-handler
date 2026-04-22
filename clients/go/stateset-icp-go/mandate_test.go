package icp

// Independent unit tests for the mandate primitives. Complements the
// cross-language golden-vector tests in vectors_test.go — if the
// fixtures go missing or become unreachable, we still exercise the
// keypair, did:key, JCS, and signing logic here against known
// in-file expectations.

import (
	"crypto/ed25519"
	"encoding/base64"
	"encoding/hex"
	"strings"
	"testing"
)

func TestDIDKeyFromPublicKey_RejectsWrongSize(t *testing.T) {
	if _, err := DIDKeyFromPublicKey([]byte{1, 2, 3}); err == nil {
		t.Error("expected error on short pubkey")
	}
}

func TestDIDKeyFormatSanity(t *testing.T) {
	// W3C did:key for Ed25519 always starts with `did:key:z6Mk`
	// (multicodec 0xed01 + 32 random bytes always base58btc-starts
	// that way — it's a property of the prefix, not the key).
	kp, err := GenerateKeyPair()
	if err != nil {
		t.Fatalf("GenerateKeyPair: %v", err)
	}
	if !strings.HasPrefix(kp.DID, "did:key:z6Mk") {
		t.Errorf("Ed25519 did:key must start with `did:key:z6Mk`, got: %s", kp.DID)
	}
	if kp.Kid != kp.DID {
		t.Errorf("default Kid should match DID: kid=%s did=%s", kp.Kid, kp.DID)
	}
}

func TestKeyPairFromSeed_DeterminismAcrossCalls(t *testing.T) {
	seed, _ := hex.DecodeString(
		"9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
	)
	a, err := KeyPairFromSeed(seed)
	if err != nil {
		t.Fatalf("KeyPairFromSeed a: %v", err)
	}
	b, err := KeyPairFromSeed(seed)
	if err != nil {
		t.Fatalf("KeyPairFromSeed b: %v", err)
	}
	if a.DID != b.DID {
		t.Errorf("same seed → different DID: %s vs %s", a.DID, b.DID)
	}
}

func TestKeyPairFromSeed_RejectsWrongSize(t *testing.T) {
	if _, err := KeyPairFromSeed([]byte{1, 2, 3}); err == nil {
		t.Error("expected error on short seed")
	}
}

func TestSignMandateRoundTrip(t *testing.T) {
	// Produce a JWS with a fresh key, then verify it with the
	// public half of that same key. If SignMandate produces
	// anything Ed25519.Verify can't check, we've drifted from JWS.
	kp, err := GenerateKeyPair()
	if err != nil {
		t.Fatalf("GenerateKeyPair: %v", err)
	}
	payload := map[string]any{
		"iss": kp.DID,
		"sub": "did:stateset:agent:test",
		"iat": int64(1745000000),
		"jti": "unit-roundtrip",
		"icp": map[string]any{
			"version": "2026-04-21",
			"scope":   []string{"quote"},
		},
	}
	jws, err := SignMandate(payload, kp)
	if err != nil {
		t.Fatalf("SignMandate: %v", err)
	}
	parts := splitJWS(jws)
	if len(parts) != 3 {
		t.Fatalf("expected 3 JWS segments, got %d", len(parts))
	}

	signingInput := parts[0] + "." + parts[1]
	sigBytes, err := decodeB64URL(parts[2])
	if err != nil {
		t.Fatalf("decode signature: %v", err)
	}
	if !ed25519.Verify(kp.Public, []byte(signingInput), sigBytes) {
		t.Error("signature produced by SignMandate does not verify against the signing key")
	}
}

func TestSignMandate_NilKeyPairRejected(t *testing.T) {
	if _, err := SignMandate(map[string]any{"iss": "x"}, nil); err == nil {
		t.Error("expected error on nil keypair")
	}
}

func TestNewMandatePayloadDefaults(t *testing.T) {
	p, err := NewMandatePayload(MandateOpts{
		Issuer:            "did:key:zAlice",
		Subject:           "did:stateset:agent:x",
		Scope:             []string{"quote"},
		BudgetCurrency:    "USD",
		BudgetAmountMinor: 10_000,
		NowSecs:           1745000000,
	})
	if err != nil {
		t.Fatalf("NewMandatePayload: %v", err)
	}
	icp := p["icp"].(map[string]any)
	if icp["version"] != "2026-04-21" {
		t.Errorf("default version wrong: %v", icp["version"])
	}
	if icp["categories"] == nil {
		t.Error("categories must default to empty slice, not nil (wire-stability)")
	}
	budget := icp["budget"].(map[string]any)
	if budget["period"] != "P1D" {
		t.Errorf("default period wrong: %v", budget["period"])
	}
	if budget["per_transaction"] != nil {
		t.Errorf("per_transaction should default to nil (null on wire), got %v", budget["per_transaction"])
	}
	// exp = nbf + ValidForSecs (default 3600)
	if p["exp"].(int64)-p["nbf"].(int64) != 3600 {
		t.Errorf("default validity window wrong: nbf=%v exp=%v", p["nbf"], p["exp"])
	}
}

func TestNewMandatePayloadRequiredFields(t *testing.T) {
	cases := map[string]MandateOpts{
		"missing issuer":   {Subject: "s", Scope: []string{"q"}, BudgetCurrency: "USD"},
		"missing subject":  {Issuer: "i", Scope: []string{"q"}, BudgetCurrency: "USD"},
		"empty scope":      {Issuer: "i", Subject: "s", BudgetCurrency: "USD"},
		"missing currency": {Issuer: "i", Subject: "s", Scope: []string{"q"}},
	}
	for name, opts := range cases {
		t.Run(name, func(t *testing.T) {
			if _, err := NewMandatePayload(opts); err == nil {
				t.Errorf("NewMandatePayload should reject %q", name)
			}
		})
	}
}

func TestBase58btcEncodeKnownAnswer(t *testing.T) {
	// Bitcoin test vector: "Hello World!" in base58btc.
	got := base58btcEncode([]byte("Hello World!"))
	if got != "2NEpo7TZRRrLZSi2U" {
		t.Errorf("base58btc(\"Hello World!\") = %q, want \"2NEpo7TZRRrLZSi2U\"", got)
	}
}

func TestBase58btcEncodeLeadingZeros(t *testing.T) {
	// Leading zero bytes map to leading '1' chars per Bitcoin convention.
	got := base58btcEncode([]byte{0, 0, 0, 0x61})
	if got != "1112g" {
		t.Errorf("base58btc(0x00000061) = %q, want \"1112g\"", got)
	}
}

func TestCanonicalJSONSortsKeysAndOmitsWhitespace(t *testing.T) {
	v := map[string]any{"z": 1, "a": 2, "m": map[string]any{"y": 3, "b": 4}}
	out, err := canonicalJSON(v)
	if err != nil {
		t.Fatalf("canonicalJSON: %v", err)
	}
	want := `{"a":2,"m":{"b":4,"y":3},"z":1}`
	if string(out) != want {
		t.Errorf("canonicalJSON mismatch\n  got:  %s\n  want: %s", string(out), want)
	}
}

// decodeB64URL decodes the raw-url-base64 signature segment — same
// alphabet SignMandate emits.
func decodeB64URL(s string) ([]byte, error) {
	return base64.RawURLEncoding.DecodeString(s)
}
