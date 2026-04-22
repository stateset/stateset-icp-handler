//! Golden-vector tests: byte-exact regression + interop fixtures.
//!
//! Each test pairs a fixed input (32-byte Ed25519 seed + payload
//! template) with expected output (did:key, JCS-canonicalized JSON,
//! compact JWS). The expected values live under
//! `docs/specification/vectors/` so non-Rust implementations can run
//! the same inputs through their code and assert byte-equality.
//!
//! Modes:
//! * **Assert** (default): read the JSON fixture; computed output must
//!   byte-match the `expected_*` fields. Used on every CI run.
//! * **Regenerate** (`ICP_REGENERATE_VECTORS=1`): compute output and
//!   write it back to the JSON fixture. Used only when an intentional
//!   wire-format change lands — review the diff before committing.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

const VECTORS_DIR: &str = "docs/specification/vectors";

// --------------------------------------------------------------------------
// Primitives
// --------------------------------------------------------------------------

fn b64url(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

fn jcs(v: &Value) -> Vec<u8> {
    serde_jcs::to_vec(v).expect("jcs canonicalize")
}

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn did_key_from_pubkey(pk: &[u8; 32]) -> String {
    let mut prefixed = Vec::with_capacity(34);
    prefixed.extend_from_slice(&[0xed, 0x01]);
    prefixed.extend_from_slice(pk);
    format!("did:key:z{}", bs58::encode(prefixed).into_string())
}

fn signing_key_from_seed(seed_hex: &str) -> SigningKey {
    let bytes = hex_decode(seed_hex);
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    SigningKey::from_bytes(&arr)
}

fn regenerate() -> bool {
    std::env::var("ICP_REGENERATE_VECTORS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn read_vectors<T: for<'de> Deserialize<'de>>(name: &str) -> T {
    let path = Path::new(VECTORS_DIR).join(name);
    let bytes = fs::read(&path).unwrap_or_else(|_| {
        panic!(
            "missing vector file {}; run with ICP_REGENERATE_VECTORS=1 to create it",
            path.display()
        )
    });
    serde_json::from_slice(&bytes).expect("parse vector file")
}

fn write_vectors<T: Serialize>(name: &str, data: &T) {
    let path = Path::new(VECTORS_DIR).join(name);
    let pretty = serde_json::to_string_pretty(data).expect("serialize vectors");
    fs::write(&path, format!("{pretty}\n")).expect("write vectors");
    eprintln!("regenerated {}", path.display());
}

// --------------------------------------------------------------------------
// did:key vectors
// --------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct DidKeyFile {
    description: String,
    vectors: Vec<DidKeyVector>,
}

#[derive(Serialize, Deserialize)]
struct DidKeyVector {
    name: String,
    public_key_hex: String,
    expected_did: String,
}

#[test]
fn did_key_encoding() {
    // RFC 8032 §7.1 test vectors 1 & 2 — deterministic, public, and
    // convenient for cross-implementation alignment.
    let inputs = [
        (
            "rfc8032_test_vector_1",
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
        ),
        (
            "rfc8032_test_vector_2",
            "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
        ),
    ];

    if regenerate() {
        let vectors = inputs
            .iter()
            .map(|(name, pk_hex)| {
                let pk_bytes = hex_decode(pk_hex);
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&pk_bytes);
                DidKeyVector {
                    name: (*name).into(),
                    public_key_hex: (*pk_hex).into(),
                    expected_did: did_key_from_pubkey(&arr),
                }
            })
            .collect();
        write_vectors(
            "did_key.json",
            &DidKeyFile {
                description: "Ed25519 public key bytes → did:key identifier (multicodec 0xed01 + base58btc multibase 'z' prefix).".into(),
                vectors,
            },
        );
        return;
    }

    let file: DidKeyFile = read_vectors("did_key.json");
    assert_eq!(file.vectors.len(), inputs.len(), "vector count mismatch");
    for (vec, (name, pk_hex)) in file.vectors.iter().zip(inputs.iter()) {
        assert_eq!(&vec.name, name);
        assert_eq!(&vec.public_key_hex, pk_hex);
        let pk_bytes = hex_decode(pk_hex);
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&pk_bytes);
        let actual = did_key_from_pubkey(&arr);
        assert_eq!(
            actual, vec.expected_did,
            "did:key mismatch for `{}` — wire format drift",
            name
        );
    }
}

// --------------------------------------------------------------------------
// Mandate JWS vectors
// --------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct MandateJwsFile {
    description: String,
    vectors: Vec<MandateJwsVector>,
}

#[derive(Serialize, Deserialize)]
struct MandateJwsVector {
    name: String,
    private_key_seed_hex: String,
    public_key_hex: String,
    issuer_did: String,
    kid: String,
    payload: Value,
    expected_header_json: String,
    expected_payload_json: String,
    expected_header_b64url: String,
    expected_payload_b64url: String,
    expected_signature_b64url: String,
    expected_compact_jws: String,
}

struct SignedJws {
    header_json: String,
    payload_json: String,
    header_b64: String,
    payload_b64: String,
    signature_b64: String,
    compact_jws: String,
}

fn sign_compact(seed_hex: &str, header: &Value, payload: &Value) -> SignedJws {
    let header_bytes = jcs(header);
    let payload_bytes = jcs(payload);
    let header_b64 = b64url(&header_bytes);
    let payload_b64 = b64url(&payload_bytes);
    let signing_input = format!("{header_b64}.{payload_b64}");
    let sk = signing_key_from_seed(seed_hex);
    let sig = sk.sign(signing_input.as_bytes());
    let signature_b64 = b64url(&sig.to_bytes());
    let compact_jws = format!("{header_b64}.{payload_b64}.{signature_b64}");
    SignedJws {
        header_json: String::from_utf8(header_bytes).unwrap(),
        payload_json: String::from_utf8(payload_bytes).unwrap(),
        header_b64,
        payload_b64,
        signature_b64,
        compact_jws,
    }
}

fn basic_buy_mandate_inputs() -> (&'static str, &'static str, Value) {
    // Fixed seed = RFC 8032 test vector 1 private key (easy to cross-check
    // against external tools). iat/nbf/exp are pinned so the whole vector
    // is reproducible — any implementer can feed the same values in and
    // must get the same JWS out.
    let seed_hex = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
    let pub_hex = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&hex_decode(pub_hex));
    let did = did_key_from_pubkey(&arr);

    let payload = json!({
        "iss": did,
        "sub": "did:stateset:agent:demo-alice",
        "iat": 1_745_000_000,
        "nbf": 1_745_000_000,
        "exp": 1_745_003_600,
        "jti": "mandate-vector-basic",
        "icp": {
            "version": "2026-04-21",
            "scope": ["quote", "authorize", "buy"],
            "budget": {
                "currency": "USD",
                "amount_minor": 50_000,
                "per_transaction": null,
                "period": "P1D",
            },
            "merchants": ["*"],
            "categories": [],
            "jurisdictions": [],
            "policies": {
                "require_receipt": true,
                "require_shipping_address_confirmation": false,
                "prohibit_subscriptions": false,
            },
            "linked_payment_methods": [],
        },
    });

    (seed_hex, pub_hex, payload)
}

#[test]
fn mandate_jws_basic_buy() {
    let (seed_hex, pub_hex, payload) = basic_buy_mandate_inputs();
    let mut pub_arr = [0u8; 32];
    pub_arr.copy_from_slice(&hex_decode(pub_hex));
    let did = did_key_from_pubkey(&pub_arr);
    let header = json!({"alg": "EdDSA", "kid": did, "typ": "JWT"});

    let signed = sign_compact(seed_hex, &header, &payload);

    if regenerate() {
        let vector = MandateJwsVector {
            name: "basic_buy_mandate".into(),
            private_key_seed_hex: seed_hex.into(),
            public_key_hex: pub_hex.into(),
            issuer_did: did.clone(),
            kid: did.clone(),
            payload: payload.clone(),
            expected_header_json: signed.header_json,
            expected_payload_json: signed.payload_json,
            expected_header_b64url: signed.header_b64,
            expected_payload_b64url: signed.payload_b64,
            expected_signature_b64url: signed.signature_b64,
            expected_compact_jws: signed.compact_jws,
        };
        write_vectors(
            "mandate_jws.json",
            &MandateJwsFile {
                description: "Compact-JWS mandate signing: Ed25519 over base64url(JCS(header)) + \".\" + base64url(JCS(payload))."
                    .into(),
                vectors: vec![vector],
            },
        );
        return;
    }

    let file: MandateJwsFile = read_vectors("mandate_jws.json");
    let vec = file
        .vectors
        .iter()
        .find(|v| v.name == "basic_buy_mandate")
        .expect("basic_buy_mandate vector missing");

    assert_eq!(vec.private_key_seed_hex, seed_hex);
    assert_eq!(vec.public_key_hex, pub_hex);
    assert_eq!(vec.issuer_did, did);
    assert_eq!(vec.payload, payload, "payload drifted from fixture");
    assert_eq!(
        signed.header_json, vec.expected_header_json,
        "header JCS bytes drifted"
    );
    assert_eq!(
        signed.payload_json, vec.expected_payload_json,
        "payload JCS bytes drifted"
    );
    assert_eq!(signed.header_b64, vec.expected_header_b64url);
    assert_eq!(signed.payload_b64, vec.expected_payload_b64url);
    assert_eq!(
        signed.signature_b64, vec.expected_signature_b64url,
        "signature drifted — Ed25519 is deterministic so this means pre-sign bytes changed"
    );
    assert_eq!(signed.compact_jws, vec.expected_compact_jws);

    // End-to-end: the handler's `decode_unverified` must accept this.
    let decoded = stateset_icp_handler::mandate::decode_unverified(&vec.expected_compact_jws)
        .expect("handler should decode vector JWS");
    assert_eq!(decoded.jti, "mandate-vector-basic");
    assert_eq!(decoded.iss, did);
}
