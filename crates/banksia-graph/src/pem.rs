//! Minimal PEM decoding for certificates and PKCS#8 private keys.

use base64::Engine;

pub fn decode(pem: &str, label: &str) -> Option<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = pem.find(&begin)? + begin.len();
    let stop = start + pem[start..].find(&end)?;
    let body: String = pem[start..stop]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    base64::engine::general_purpose::STANDARD.decode(body).ok()
}

pub fn encode(der: &[u8], label: &str) -> String {
    let b = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in b.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).expect("base64 is ascii"));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        let pem = super::encode(&[1, 2, 3, 250], "CERTIFICATE");
        assert_eq!(
            super::decode(&pem, "CERTIFICATE").unwrap(),
            vec![1, 2, 3, 250]
        );
        assert!(super::decode(&pem, "PRIVATE KEY").is_none());
    }
}
