package icp

// Cross-language interop tests: load the shared golden fixtures from
// `docs/specification/vectors/` and assert the Go implementation of
// did:key encoding and compact-JWS mandate signing produces
// byte-identical output to the Rust reference and the Python client.
//
// Ed25519 is deterministic (RFC 8032 §5.1.6), so signatures match
// iff the pre-sign bytes match iff JCS canonicalization is consistent
// across implementations. Passing these tests is evidence of
// byte-identical interop.

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

// vectorsDir resolves `<repo>/docs/specification/vectors/` from this
// test's cwd (`clients/go/stateset-icp-go`). Three `..` climbs.
func vectorsDir(t *testing.T) string {
	t.Helper()
	p := filepath.Join("..", "..", "..", "docs", "specification", "vectors")
	info, err := os.Stat(p)
	if err != nil || !info.IsDir() {
		t.Skipf("vector directory not found at %s; skipping cross-language check", p)
	}
	return p
}

// ---- did:key ---------------------------------------------------------

func TestVectorsDidKey(t *testing.T) {
	raw, err := os.ReadFile(filepath.Join(vectorsDir(t), "did_key.json"))
	if err != nil {
		t.Fatalf("read did_key.json: %v", err)
	}
	var file struct {
		Vectors []struct {
			Name         string `json:"name"`
			PublicKeyHex string `json:"public_key_hex"`
			ExpectedDID  string `json:"expected_did"`
		} `json:"vectors"`
	}
	if err := json.Unmarshal(raw, &file); err != nil {
		t.Fatalf("parse did_key.json: %v", err)
	}
	if len(file.Vectors) == 0 {
		t.Fatal("did_key.json has no vectors")
	}
	for _, v := range file.Vectors {
		pk, err := hex.DecodeString(v.PublicKeyHex)
		if err != nil {
			t.Fatalf("decode pubkey for %s: %v", v.Name, err)
		}
		got, err := DIDKeyFromPublicKey(pk)
		if err != nil {
			t.Fatalf("%s: DIDKeyFromPublicKey: %v", v.Name, err)
		}
		if got != v.ExpectedDID {
			t.Errorf("%s: did:key mismatch\n  got:  %s\n  want: %s", v.Name, got, v.ExpectedDID)
		}
	}
}

// ---- mandate JWS -----------------------------------------------------

func TestVectorsMandateJWS(t *testing.T) {
	raw, err := os.ReadFile(filepath.Join(vectorsDir(t), "mandate_jws.json"))
	if err != nil {
		t.Fatalf("read mandate_jws.json: %v", err)
	}

	// Use UseNumber so JSON integers stay as json.Number (preserving
	// their original decimal form) rather than float64. If we let
	// the default decoder turn `iat: 1745000000` into a float64, a
	// subsequent re-Marshal would emit it as `1.745e+09` — which
	// would flip every byte after the first seven in the payload
	// b64url segment, breaking the signature match.
	dec := json.NewDecoder(bytes.NewReader(raw))
	dec.UseNumber()
	var file struct {
		Vectors []struct {
			Name                    string         `json:"name"`
			PrivateKeySeedHex       string         `json:"private_key_seed_hex"`
			PublicKeyHex            string         `json:"public_key_hex"`
			IssuerDID               string         `json:"issuer_did"`
			Kid                     string         `json:"kid"`
			Payload                 map[string]any `json:"payload"`
			ExpectedHeaderB64URL    string         `json:"expected_header_b64url"`
			ExpectedPayloadB64URL   string         `json:"expected_payload_b64url"`
			ExpectedSignatureB64URL string         `json:"expected_signature_b64url"`
			ExpectedCompactJWS      string         `json:"expected_compact_jws"`
		} `json:"vectors"`
	}
	if err := dec.Decode(&file); err != nil {
		t.Fatalf("parse mandate_jws.json: %v", err)
	}
	if len(file.Vectors) == 0 {
		t.Fatal("mandate_jws.json has no vectors")
	}

	for _, v := range file.Vectors {
		seed, err := hex.DecodeString(v.PrivateKeySeedHex)
		if err != nil {
			t.Fatalf("%s: decode seed: %v", v.Name, err)
		}
		kp, err := KeyPairFromSeed(seed)
		if err != nil {
			t.Fatalf("%s: KeyPairFromSeed: %v", v.Name, err)
		}
		if kp.DID != v.IssuerDID {
			t.Errorf("%s: did:key mismatch\n  got:  %s\n  want: %s", v.Name, kp.DID, v.IssuerDID)
		}
		if kp.Kid != v.Kid {
			t.Errorf("%s: kid mismatch\n  got:  %s\n  want: %s", v.Name, kp.Kid, v.Kid)
		}

		jws, err := SignMandate(v.Payload, kp)
		if err != nil {
			t.Fatalf("%s: SignMandate: %v", v.Name, err)
		}

		// Decompose the produced JWS so a failure points at the
		// specific segment that drifted (header, payload, or sig).
		parts := splitJWS(jws)
		if len(parts) != 3 {
			t.Fatalf("%s: produced JWS has %d segments, want 3", v.Name, len(parts))
		}
		if parts[0] != v.ExpectedHeaderB64URL {
			t.Errorf("%s: header b64url drift\n  got:  %s\n  want: %s", v.Name, parts[0], v.ExpectedHeaderB64URL)
		}
		if parts[1] != v.ExpectedPayloadB64URL {
			t.Errorf("%s: payload b64url drift (canonicalization differs from Rust/Python)", v.Name)
		}
		if parts[2] != v.ExpectedSignatureB64URL {
			t.Errorf("%s: signature drift — Ed25519 is deterministic so this means the pre-sign bytes differ", v.Name)
		}

		if jws != v.ExpectedCompactJWS {
			t.Errorf("%s: compact JWS mismatch", v.Name)
		}
	}
}

func splitJWS(s string) []string {
	var parts []string
	start := 0
	for i := 0; i < len(s); i++ {
		if s[i] == '.' {
			parts = append(parts, s[start:i])
			start = i + 1
		}
	}
	parts = append(parts, s[start:])
	return parts
}
