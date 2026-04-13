use base64::Engine;
use lru::LruCache;
use parking_lot::Mutex;
use std::num::NonZeroUsize;
use std::time::Instant;

// DIFFERENTIATOR 3: blockhash freshness check for sendTransaction.
//
// Rolling LRU cache of (blockhash -> slot first seen). On sendTransaction we
// extract the tx's recent_blockhash and reject if:
//   - the blockhash isn't in our cache (after warmup), or
//   - tip - seen_slot > max_blockhash_age_slots.
// The cache is populated by:
//   - health-loop getLatestBlockhash calls
//   - opportunistic harvesting of client-issued getLatestBlockhash responses

pub struct BlockhashCache {
    inner: Mutex<LruCache<String, u64>>,
    created_at: Instant,
    warmup_secs: u64,
}

impl BlockhashCache {
    pub fn new(warmup_secs: u64, max_entries: usize) -> Self {
        let cap = NonZeroUsize::new(max_entries.max(1)).expect("max_entries > 0");
        Self {
            inner: Mutex::new(LruCache::new(cap)),
            created_at: Instant::now(),
            warmup_secs,
        }
    }

    pub fn insert(&self, blockhash: String, slot: u64) {
        let mut c = self.inner.lock();
        // Preserve the highest slot we've seen for this blockhash. `put` also
        // promotes it to the head of the LRU, keeping fresh hashes alive.
        let new_slot = c.peek(&blockhash).copied().unwrap_or(0).max(slot);
        c.put(blockhash, new_slot);
    }

    pub fn get_slot(&self, blockhash: &str) -> Option<u64> {
        self.inner.lock().get(blockhash).copied()
    }

    pub fn is_warmed_up(&self) -> bool {
        self.created_at.elapsed().as_secs() >= self.warmup_secs
    }
}

impl Default for BlockhashCache {
    fn default() -> Self {
        Self::new(30, 512)
    }
}

/// Read Solana's compact-u16 (shortvec) encoding. Returns (value, bytes_consumed).
fn read_shortvec(bytes: &[u8]) -> Option<(u16, usize)> {
    let mut val: u32 = 0;
    let mut shift: u32 = 0;
    for i in 0..3 {
        let b = *bytes.get(i)?;
        val |= ((b & 0x7f) as u32) << shift;
        if b & 0x80 == 0 {
            if val > u16::MAX as u32 {
                return None;
            }
            return Some((val as u16, i + 1));
        }
        shift += 7;
    }
    None
}

/// Extract the recent_blockhash from a serialized (legacy or v0) transaction.
/// Layout: [sigs shortvec][sigs*64][optional version byte][3 hdr bytes]
///         [accts shortvec][accts*32][recent_blockhash 32B][...]
pub fn extract_blockhash(tx_bytes: &[u8]) -> Option<String> {
    let mut cursor = 0;
    let (sig_count, n) = read_shortvec(tx_bytes.get(cursor..)?)?;
    cursor += n;
    cursor += (sig_count as usize) * 64;
    if cursor >= tx_bytes.len() {
        return None;
    }

    // v0+ transactions have a high-bit-set version prefix byte
    let first = tx_bytes[cursor];
    if first & 0x80 != 0 {
        cursor += 1;
    }
    // message header: 3 bytes (num_required_sigs, num_readonly_signed, num_readonly_unsigned)
    cursor += 3;
    let (acct_count, n) = read_shortvec(tx_bytes.get(cursor..)?)?;
    cursor += n;
    cursor += (acct_count as usize) * 32;
    if cursor + 32 > tx_bytes.len() {
        return None;
    }
    Some(bs58::encode(&tx_bytes[cursor..cursor + 32]).into_string())
}

/// Decode a sendTransaction `params[0]` string per the `encoding` option.
pub fn decode_tx(data: &str, encoding: &str) -> Option<Vec<u8>> {
    match encoding {
        "base58" => bs58::decode(data).into_vec().ok(),
        _ => base64::engine::general_purpose::STANDARD.decode(data).ok(),
    }
}

#[cfg(test)]
mod tests;
