//! enc1103: Apple's encrypted RFB record layer.
//!
//! After auth the server sends a 36-byte blob (encoding 1103) holding an
//! AES-128 key and IV, each ECB-wrapped under the auth key. From the
//! PostEncryptionToggle onwards every message in both directions is framed
//! as:
//!
//! ```text
//! u16 BE ciphertext_len || AES-128-CBC( u16 BE len || msg || zero pad || SHA1(u32 BE seq || preceding bytes) )
//! ```
//!
//! Each direction keeps its own CBC chaining state and sequence counter.

use std::io::{self, Read};

use aes::Aes128;
use aes::cipher::{BlockDecrypt, BlockDecryptMut, BlockEncryptMut, KeyInit, KeyIvInit, generic_array::GenericArray};
use sha1::{Digest, Sha1};

const MAC_LEN: usize = 20;
const BLOCK: usize = 16;
/// How far ahead of the expected receive counter a MAC may match. The server
/// occasionally skips counter values.
const RECV_WINDOW: u32 = 6;
/// The most message bytes one record carries: 65,498. The u16 ciphertext
/// length holds whole cipher blocks, so a record has at most 65,520 bytes for
/// the message length, the message, and the MAC.
const MAX_RECORD_MSG: usize = u16::MAX as usize / BLOCK * BLOCK - 2 - MAC_LEN;

type CbcEnc = cbc::Encryptor<Aes128>;
type CbcDec = cbc::Decryptor<Aes128>;

/// Record-layer key material unwrapped from the 1103 blob.
#[derive(Clone)]
pub struct RecordKeys {
    pub key: [u8; 16],
    pub iv: [u8; 16],
}

impl RecordKeys {
    /// Unwraps the 36-byte 1103 payload (`u32 generation || ecb(key) || ecb(iv)`).
    pub fn unwrap(blob: &[u8], auth_key: &[u8; 16]) -> Option<Self> {
        if blob.len() < 36 {
            return None;
        }
        let ecb = Aes128::new(GenericArray::from_slice(auth_key));
        let mut key = [0u8; 16];
        let mut iv = [0u8; 16];
        key.copy_from_slice(&blob[4..20]);
        iv.copy_from_slice(&blob[20..36]);
        ecb.decrypt_block(GenericArray::from_mut_slice(&mut key));
        ecb.decrypt_block(GenericArray::from_mut_slice(&mut iv));
        Some(Self { key, iv })
    }
}

fn mac(seq: u32, data: &[u8]) -> [u8; MAC_LEN] {
    let mut h = Sha1::new();
    h.update(seq.to_be_bytes());
    h.update(data);
    h.finalize().into()
}

/// Outbound half: encrypts messages into wire records.
pub struct RecordSealer {
    cbc: CbcEnc,
    seq: u32,
}

impl RecordSealer {
    pub fn new(keys: &RecordKeys) -> Self {
        Self { cbc: CbcEnc::new(&keys.key.into(), &keys.iv.into()), seq: 0 }
    }

    /// Wraps one message into as many records as it needs, each with at most
    /// [`MAX_RECORD_MSG`] bytes. Only clipboard messages need more than one.
    pub fn seal(&mut self, msg: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(msg.len() + 2 + 2 + BLOCK + MAC_LEN);
        let mut rest = msg;
        loop {
            let (head, tail) = rest.split_at(rest.len().min(MAX_RECORD_MSG));
            self.seal_record(head, &mut out);
            rest = tail;
            if rest.is_empty() {
                return out;
            }
        }
    }

    fn seal_record(&mut self, msg: &[u8], out: &mut Vec<u8>) {
        let pad = (BLOCK - (2 + msg.len() + MAC_LEN) % BLOCK) % BLOCK;
        let ct_len = 2 + msg.len() + pad + MAC_LEN;
        let start = out.len();
        out.extend_from_slice(&(ct_len as u16).to_be_bytes());
        out.extend_from_slice(&(msg.len() as u16).to_be_bytes());
        out.extend_from_slice(msg);
        out.resize(out.len() + pad, 0);
        let tag = mac(self.seq, &out[start + 2..]);
        out.extend_from_slice(&tag);
        for block in out[start + 2..].as_chunks_mut::<BLOCK>().0 {
            self.cbc.encrypt_block_mut(GenericArray::from_mut_slice(block));
        }
        self.seq = self.seq.wrapping_add(1);
    }
}

/// Inbound half: reads and decrypts records from a byte stream.
pub struct RecordOpener {
    cbc: CbcDec,
    seq: u32,
}

/// Result of reading one record.
pub enum Opened {
    /// The decrypted message body.
    Message(Vec<u8>),
    /// The record decrypted but no counter in the window matched its MAC.
    /// The CBC state still advanced, so the stream stays in sync.
    BadMac,
}

impl RecordOpener {
    pub fn new(keys: &RecordKeys) -> Self {
        Self { cbc: CbcDec::new(&keys.key.into(), &keys.iv.into()), seq: 0 }
    }

    /// Blocks until a full record arrives and returns its plaintext.
    pub fn read(&mut self, r: &mut impl Read) -> io::Result<Opened> {
        let mut len = [0u8; 2];
        r.read_exact(&mut len)?;
        let len = u16::from_be_bytes(len) as usize;
        if len == 0 || !len.is_multiple_of(BLOCK) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad record length {len}"),
            ));
        }
        let mut ct = vec![0u8; len];
        r.read_exact(&mut ct)?;
        Ok(self.open(ct))
    }

    fn open(&mut self, mut buf: Vec<u8>) -> Opened {
        for block in buf.as_chunks_mut::<BLOCK>().0 {
            self.cbc.decrypt_block_mut(GenericArray::from_mut_slice(block));
        }
        let expected = self.seq;
        self.seq = self.seq.wrapping_add(1);
        if buf.len() <= MAC_LEN + 2 {
            return Opened::BadMac;
        }
        let (body, tag) = buf.split_at(buf.len() - MAC_LEN);
        let first = expected.saturating_sub(1);
        for seq in first..expected + RECV_WINDOW {
            if mac(seq, body) == tag {
                self.seq = seq.wrapping_add(1);
                let n = (u16::from_be_bytes([body[0], body[1]]) as usize).min(body.len() - 2);
                return Opened::Message(body[2..2 + n].to_vec());
            }
        }
        Opened::BadMac
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> RecordKeys {
        RecordKeys { key: [7u8; 16], iv: [9u8; 16] }
    }

    #[test]
    fn round_trip_keeps_chaining_and_counters() {
        let mut tx = RecordSealer::new(&keys());
        let mut rx = RecordOpener::new(&keys());
        let msgs: [&[u8]; 4] = [b"", b"hello", &[0xAB; 33], &[1u8; 300]];
        let mut wire = Vec::new();
        for m in msgs {
            let rec = tx.seal(m);
            assert_eq!((rec.len() - 2) % 16, 0);
            wire.extend_from_slice(&rec);
        }
        let mut cur = io::Cursor::new(wire);
        for m in msgs {
            match rx.read(&mut cur).unwrap() {
                Opened::Message(got) => assert_eq!(got, m),
                Opened::BadMac => panic!("mac mismatch"),
            }
        }
    }

    fn open_all(rx: &mut RecordOpener, wire: Vec<u8>) -> Vec<Vec<u8>> {
        let len = wire.len() as u64;
        let mut cur = io::Cursor::new(wire);
        let mut msgs = Vec::new();
        while cur.position() < len {
            match rx.read(&mut cur).unwrap() {
                Opened::Message(m) => msgs.push(m),
                Opened::BadMac => panic!("mac mismatch"),
            }
        }
        msgs
    }

    #[test]
    fn message_of_65498_bytes_fills_one_record() {
        assert_eq!(MAX_RECORD_MSG, 65_498);
        let msg = vec![0x1f; 65_498];
        let wire = RecordSealer::new(&keys()).seal(&msg);
        assert_eq!(wire.len(), 2 + 65_520);
        assert_eq!(open_all(&mut RecordOpener::new(&keys()), wire), [msg]);
    }

    #[test]
    fn message_of_65499_bytes_splits_into_two_records() {
        let msg: Vec<u8> = (0..65_499u32).map(|i| i as u8).collect();
        let mut tx = RecordSealer::new(&keys());
        let mut rx = RecordOpener::new(&keys());
        let got = open_all(&mut rx, tx.seal(&msg));
        assert_eq!(got.iter().map(Vec::len).collect::<Vec<_>>(), [65_498, 1]);
        assert_eq!(got.concat(), msg);
        // Both directions count the second record.
        assert_eq!((tx.seq, rx.seq), (2, 2));
        let next = tx.seal(b"next");
        assert_eq!(open_all(&mut rx, next), [b"next".to_vec()]);
    }

    #[test]
    fn tolerates_skipped_sequence_numbers() {
        let mut tx = RecordSealer::new(&keys());
        let mut rx = RecordOpener::new(&keys());
        tx.seq = 3; // the server skipped three counter values
        let rec = tx.seal(b"skip");
        let mut cur = io::Cursor::new(rec);
        assert!(matches!(rx.read(&mut cur).unwrap(), Opened::Message(m) if m == b"skip"));
        assert_eq!(rx.seq, 4);
    }

    #[test]
    fn unwraps_key_blob() {
        let auth = [3u8; 16];
        let ecb = Aes128::new(GenericArray::from_slice(&auth));
        let mut blob = vec![0u8; 36];
        let mut k = GenericArray::clone_from_slice(&[1u8; 16]);
        let mut v = GenericArray::clone_from_slice(&[2u8; 16]);
        use aes::cipher::BlockEncrypt;
        ecb.encrypt_block(&mut k);
        ecb.encrypt_block(&mut v);
        blob[4..20].copy_from_slice(&k);
        blob[20..36].copy_from_slice(&v);
        let keys = RecordKeys::unwrap(&blob, &auth).unwrap();
        assert_eq!(keys.key, [1u8; 16]);
        assert_eq!(keys.iv, [2u8; 16]);
    }
}
