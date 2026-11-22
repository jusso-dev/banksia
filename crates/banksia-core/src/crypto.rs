//! Repository cryptography.
//!
//! A 256-bit master key, held by the customer, derives three independent keys
//! with HKDF-SHA256: one for authenticated encryption, one for object
//! identifiers and one for evidence signing. Objects are sealed with
//! XChaCha20-Poly1305 using a random 192-bit nonce, and the object's repository
//! path is bound in as associated data.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::fmt;
use zeroize::{Zeroize, Zeroizing};

type HmacSha256 = Hmac<Sha256>;

const MAGIC: &[u8; 4] = b"BKS1";
const NONCE_LEN: usize = 24;
const HKDF_SALT: &[u8] = b"banksia repository v1";

pub fn fill_random(buf: &mut [u8]) {
    getrandom::fill(buf).expect("operating system random number generator failed");
}

/// The customer-held master key.
pub struct MasterKey(Zeroizing<[u8; 32]>);

impl MasterKey {
    pub fn generate() -> Self {
        let mut k = Zeroizing::new([0u8; 32]);
        fill_random(k.as_mut());
        Self(k)
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Parse the key file format: base64 of 32 bytes, optionally prefixed with
    /// `banksia-key-v1:` and surrounded by whitespace.
    pub fn parse(text: &str) -> Result<Self> {
        let t = text.trim();
        let t = t.strip_prefix("banksia-key-v1:").unwrap_or(t);
        let mut raw = Zeroizing::new(
            base64::engine::general_purpose::STANDARD
                .decode(t)
                .map_err(|_| anyhow!("master key is not valid base64"))?,
        );
        if raw.len() != 32 {
            bail!("master key must be 32 bytes");
        }
        let mut k = [0u8; 32];
        k.copy_from_slice(&raw);
        raw.zeroize();
        Ok(Self::from_bytes(k))
    }

    /// Text form for a key file. Handle with care: this is the secret.
    pub fn to_key_file(&self) -> Zeroizing<String> {
        Zeroizing::new(format!(
            "banksia-key-v1:{}\n",
            base64::engine::general_purpose::STANDARD.encode(self.0.as_ref())
        ))
    }

    pub fn derive(&self) -> Keys {
        let hk = hkdf::Hkdf::<Sha256>::new(Some(HKDF_SALT), self.0.as_ref());
        let mut enc = Zeroizing::new([0u8; 32]);
        let mut id = Zeroizing::new([0u8; 32]);
        let mut evidence = Zeroizing::new([0u8; 32]);
        hk.expand(b"banksia encryption", enc.as_mut())
            .expect("hkdf length");
        hk.expand(b"banksia identifiers", id.as_mut())
            .expect("hkdf length");
        hk.expand(b"banksia evidence", evidence.as_mut())
            .expect("hkdf length");
        Keys { enc, id, evidence }
    }
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MasterKey([REDACTED])")
    }
}

/// Derived keys. Cheap to clone; zeroised on drop.
#[derive(Clone)]
pub struct Keys {
    enc: Zeroizing<[u8; 32]>,
    id: Zeroizing<[u8; 32]>,
    evidence: Zeroizing<[u8; 32]>,
}

impl fmt::Debug for Keys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Keys([REDACTED])")
    }
}

impl Keys {
    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(&chacha20poly1305::Key::from(*self.enc))
    }

    /// Seal `plaintext` for storage at `path`.
    pub fn seal(&self, path: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_LEN];
        fill_random(&mut nonce);
        let aad = aad(path);
        let ct = self
            .cipher()
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow!("encryption failed"))?;
        let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + ct.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Open an object read from `path`. Fails on any tampering, wrong key or
    /// an object moved from another path.
    pub fn open(&self, path: &str, sealed: &[u8]) -> Result<Vec<u8>> {
        if sealed.len() < MAGIC.len() + NONCE_LEN + 16 || &sealed[..4] != MAGIC {
            bail!("object {path} is not a Banksia object");
        }
        let nonce: [u8; NONCE_LEN] = sealed[4..4 + NONCE_LEN].try_into().expect("length checked");
        let aad = aad(path);
        self.cipher()
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &sealed[4 + NONCE_LEN..],
                    aad: &aad,
                },
            )
            .map_err(|_| {
                anyhow!("object {path} failed authentication (tampered, moved or wrong key)")
            })
    }

    /// Keyed identifier: HMAC-SHA256 under the identifier key, hex encoded.
    pub fn id_hex(&self, domain: &str, data: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(self.id.as_ref()).expect("hmac key");
        mac.update(domain.as_bytes());
        mac.update(&[0]);
        mac.update(data);
        hex::encode(mac.finalize().into_bytes())
    }

    /// Opaque, stable tag for a principal (for example an Entra object id).
    pub fn principal_tag(&self, object_id: &str) -> String {
        self.id_hex("principal", object_id.to_ascii_lowercase().as_bytes())[..32].to_string()
    }

    /// Content id for a chunk.
    pub fn blob_id(&self, chunk: &[u8]) -> String {
        self.id_hex("blob", chunk)
    }

    /// Value stored in the repository config to detect a wrong key early.
    pub fn key_check(&self) -> String {
        self.id_hex("key check", b"banksia")
    }

    /// HMAC-SHA256 under the evidence key, hex encoded.
    pub fn evidence_mac(&self, data: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(self.evidence.as_ref()).expect("hmac key");
        mac.update(data);
        hex::encode(mac.finalize().into_bytes())
    }

    pub fn verify_evidence_mac(&self, data: &[u8], expected_hex: &str) -> Result<()> {
        let expected = hex::decode(expected_hex.trim()).context("evidence signature is not hex")?;
        let mut mac = HmacSha256::new_from_slice(self.evidence.as_ref()).expect("hmac key");
        mac.update(data);
        mac.verify_slice(&expected)
            .map_err(|_| anyhow!("evidence signature does not match"))
    }
}

fn aad(path: &str) -> Vec<u8> {
    let mut v = b"banksia object v1\0".to_vec();
    v.extend_from_slice(path.as_bytes());
    v
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Incremental SHA-256 for streamed content.
#[derive(Default, Clone)]
pub struct Sha256Stream(Sha256);

impl Sha256Stream {
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
    pub fn finish_hex(self) -> String {
        hex::encode(self.0.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_round_trip() {
        let k = MasterKey::generate().derive();
        let ct = k.seal("blobs/aa/x", b"hello").unwrap();
        assert_eq!(k.open("blobs/aa/x", &ct).unwrap(), b"hello");
    }

    #[test]
    fn tamper_moved_and_wrong_key_fail() {
        let k = MasterKey::generate().derive();
        let mut ct = k.seal("a", b"hello").unwrap();
        assert!(k.open("b", &ct).is_err(), "moved object must fail");
        let other = MasterKey::generate().derive();
        assert!(other.open("a", &ct).is_err(), "wrong key must fail");
        let last = ct.len() - 1;
        ct[last] ^= 1;
        assert!(k.open("a", &ct).is_err(), "tampered object must fail");
    }

    #[test]
    fn key_file_round_trip_and_stable_ids() {
        let m = MasterKey::generate();
        let text = m.to_key_file();
        let m2 = MasterKey::parse(&text).unwrap();
        let (a, b) = (m.derive(), m2.derive());
        assert_eq!(a.blob_id(b"x"), b.blob_id(b"x"));
        assert_eq!(a.principal_tag("ABC"), b.principal_tag("abc"));
        assert_ne!(a.blob_id(b"x"), a.id_hex("principal", b"x"));
        assert!(MasterKey::parse("short").is_err());
    }

    #[test]
    fn evidence_mac_verifies() {
        let k = MasterKey::generate().derive();
        let mac = k.evidence_mac(b"manifest");
        assert!(k.verify_evidence_mac(b"manifest", &mac).is_ok());
        assert!(k.verify_evidence_mac(b"manifest!", &mac).is_err());
    }

    #[test]
    fn debug_never_prints_keys() {
        let m = MasterKey::generate();
        assert_eq!(format!("{m:?}"), "MasterKey([REDACTED])");
        assert_eq!(format!("{:?}", m.derive()), "Keys([REDACTED])");
    }
}
