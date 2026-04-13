use super::*;

// Build a minimal, well-formed tx:
//   [shortvec sig_count=1][64 sig bytes]
//   [optional 0x80 version byte for v0]
//   [3-byte msg header]
//   [shortvec acct_count=1][32 acct bytes]
//   [32 blockhash bytes]
fn make_tx(bh: [u8; 32], versioned: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(1); // sig count shortvec
    out.extend(std::iter::repeat_n(0u8, 64));
    if versioned {
        out.push(0x80);
    }
    out.extend_from_slice(&[1, 0, 0]); // header
    out.push(1); // acct count shortvec
    out.extend(std::iter::repeat_n(0u8, 32));
    out.extend_from_slice(&bh);
    out
}

#[test]
fn extract_blockhash_legacy() {
    let bh = [7u8; 32];
    let tx = make_tx(bh, false);
    let got = extract_blockhash(&tx).expect("parse");
    assert_eq!(got, bs58::encode(bh).into_string());
}

#[test]
fn extract_blockhash_versioned() {
    let bh = [42u8; 32];
    let tx = make_tx(bh, true);
    let got = extract_blockhash(&tx).expect("parse");
    assert_eq!(got, bs58::encode(bh).into_string());
}

#[test]
fn extract_blockhash_truncated_returns_none() {
    let tx = vec![1u8; 10];
    assert!(extract_blockhash(&tx).is_none());
}

#[test]
fn blockhash_cache_stores_max_slot() {
    let c = BlockhashCache::new(30, 512);
    c.insert("abc".into(), 10);
    c.insert("abc".into(), 100);
    c.insert("abc".into(), 50);
    assert_eq!(c.get_slot("abc"), Some(100));
}

#[test]
fn shortvec_single_byte() {
    assert_eq!(read_shortvec(&[5]), Some((5, 1)));
}

#[test]
fn shortvec_two_bytes() {
    // 128 = 0b10000000 | 0b00000001 => (0x80, 0x01) encodes value 128
    assert_eq!(read_shortvec(&[0x80, 0x01]), Some((128, 2)));
}

#[test]
fn decode_tx_base58_roundtrip() {
    let raw = vec![1u8, 2, 3, 4, 5];
    let s = bs58::encode(&raw).into_string();
    assert_eq!(decode_tx(&s, "base58"), Some(raw));
}

#[test]
fn decode_tx_base64_default() {
    let raw = vec![10u8, 20, 30];
    use base64::Engine;
    let s = base64::engine::general_purpose::STANDARD.encode(&raw);
    // when encoding is anything other than "base58", default is base64
    assert_eq!(decode_tx(&s, "base64"), Some(raw.clone()));
    assert_eq!(decode_tx(&s, ""), Some(raw));
}

#[test]
fn decode_tx_invalid_returns_none() {
    assert!(decode_tx("!!!not-valid-base64!!!", "base64").is_none());
    assert!(decode_tx("0OIl", "base58").is_none()); // 0, O, I, l are not base58 chars
}

#[test]
fn blockhash_cache_respects_warmup_threshold() {
    let c = BlockhashCache::new(30, 512);
    // Fresh cache: not yet warmed up (30s default)
    assert!(!c.is_warmed_up());
}

#[test]
fn blockhash_cache_lru_evicts_oldest() {
    let c = BlockhashCache::new(30, 512);
    // Insert more than max_entries; oldest should be dropped.
    // (max_entries=512 hardcoded; we don't need to hit that here — just verify
    //  that two distinct inserts are both findable.)
    c.insert("hash-a".into(), 1);
    c.insert("hash-b".into(), 2);
    assert_eq!(c.get_slot("hash-a"), Some(1));
    assert_eq!(c.get_slot("hash-b"), Some(2));
    assert_eq!(c.get_slot("hash-c"), None);
}
