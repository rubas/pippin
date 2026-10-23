//! RFB security type 33 (0x21): Apple's SRP-6a login with the RFC 5054
//! 4096-bit group, SHA-512, and PBKDF2.
//!
//! The login starts with an "RSA1 init" request that fetches the server's
//! RSA public key. That key only hides the user name, because nothing
//! authenticates it. The SRP proofs authenticate both sides: M1 proves that
//! we know the password, and M2 proves that the host holds its verifier. The
//! result is a 16-byte AES key that later unwraps the enc1103 record-layer
//! keys.
//!
//! The SRP messages are the SASL SRP buffers of Apple's `srp.c`
//! (`MakeBuffer`): `u32 len || fields`, where `%m` is `u16 len || bytes`,
//! `%o` is `u8 len || bytes`, and `%s` is `u16 len || UTF-8`. screensharingd
//! puts a 6-byte header in front of each buffer.

use std::io::{Read, Write};
use std::sync::LazyLock;

use aes::Aes128;
use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
use anyhow::{Context, Result, bail};
use num_bigint::BigUint;
use num_traits::Zero;
use rand::RngCore;
use rand::rngs::OsRng;
use rsa::pkcs8::DecodePublicKey;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};
use sha2::{Digest, Sha256, Sha512};
use subtle::ConstantTimeEq;
use thiserror::Error;

use super::wire::{Cursor, read_u32, read_vec};

const SRP_MODULUS_LEN: usize = 512;
const SRP_HASH_LEN: u8 = 64;
const SRP_DK_LEN: usize = 128;
const SRP_MAX_ITERATIONS: u64 = 1_000_000;
const RSA_BLOCK_LEN: usize = 256;
/// The largest auth message we read. Apple's challenge is 1165 bytes and its
/// RSA key reply about 300.
const MAX_AUTH_MSG: usize = 8192;
/// The SASL buffer starts after this many bytes of screensharingd header.
const SASL_OFFSET: usize = 6;

/// The RFC 5054 4096-bit prime (the same prime as the RFC 3526 4096-bit
/// MODP group). Apple's SASL plugin knows the RFC 5054 groups from 1024 to
/// 8192 bits; screensharingd uses this one, with g = 5.
const SRP_N_HEX: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74",
    "020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437",
    "4FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF05",
    "98DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB",
    "9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B",
    "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF695581718",
    "3995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33",
    "A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864",
    "D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E2",
    "08E24FA074E5AB3143DB5BFCE0FD108E4B82D120A92108011A723C12A787E6D7",
    "88719A10BDBA5B2699C327186AF4E23C1A946834B6150BDA2583E9CA2AD44CE8",
    "DBBBC2DB04DE8EF92E8EFC141FBECAA6287C59474E6BC05D99B2964FA090C3A2",
    "233BA186515BE7ED1F612970CEE2D7AFB81BDD762170481CD0069127D5B05AA9",
    "93B4EA988D8FDDC186FFB7DC90A6C08F4DF435C934063199FFFFFFFFFFFFFFFF",
);
const SRP_G: u32 = 5;

/// The one SRP group we accept, with the values that depend only on it.
struct Group {
    n: BigUint,
    g: BigUint,
    /// `H(N) XOR H(PAD(g))`, the head of M1.
    h_ng: [u8; 64],
    /// The SRP-6a multiplier `k = H(N || PAD(g))`.
    k: BigUint,
}

static GROUP: LazyLock<Group> = LazyLock::new(|| {
    let n = BigUint::parse_bytes(SRP_N_HEX.as_bytes(), 16).expect("valid hex");
    let g = BigUint::from(SRP_G);
    let (n_pad, g_pad) = (pad(&n), pad(&g));
    let (h_n, h_g) = (sha512(&[&n_pad]), sha512(&[&g_pad]));
    let k = BigUint::from_bytes_be(&sha512(&[&n_pad, &g_pad]));
    Group { h_ng: std::array::from_fn(|i| h_n[i] ^ h_g[i]), k, n, g }
});

/// Why the login failed.
#[derive(Debug, Error)]
pub enum AuthError {
    #[error("authentication rejected by server (result {0})")]
    Rejected(u32),
    /// The host accepted our proof, but its own proof was wrong. Only a
    /// host without the account's verifier sends that: an impostor.
    #[error("the host did not prove that it knows the password (wrong SRP server proof)")]
    ServerProof,
}

/// Why we refused an SRP challenge.
#[derive(Debug, Error, PartialEq, Eq)]
enum BadChallenge {
    #[error("malformed")]
    Malformed,
    #[error("not Apple's SRP group (RFC 5054 4096-bit, g = 5)")]
    Group,
    #[error("server public value B is 0 or not below N")]
    PublicValue,
    #[error("empty salt")]
    Salt,
    #[error("iteration count {0} is not between 1 and {SRP_MAX_ITERATIONS}")]
    Iterations(u64),
}

/// Reads one `u32 len || body` auth message.
fn read_msg<S: Read>(sock: &mut S) -> Result<Vec<u8>> {
    let len = read_u32(sock)? as usize;
    if len > MAX_AUTH_MSG {
        bail!("auth message of {len} bytes is larger than {MAX_AUTH_MSG}");
    }
    Ok(read_vec(sock, len)?)
}

/// Requests the server's RSA public key. The leading 0x21 byte doubles as
/// our choice of security type.
fn rsa1_init<S: Read + Write>(sock: &mut S) -> Result<RsaPublicKey> {
    sock.write_all(b"\x21\x00\x00\x00\x0a\x01\x00RSA1\x00\x00\x00\x00")?;
    let pkt = read_msg(sock)?;
    let mut c = Cursor::at(&pkt, 2);
    let key_len = c.u32().context("short RSA1 reply")? as usize;
    let der = c.take(key_len).context("truncated RSA1 public key")?;
    RsaPublicKey::from_public_key_der(der).context("parsing server RSA key")
}

fn aes_ecb_encrypt(key: &[u8], data: &mut [u8]) {
    let cipher = Aes128::new(GenericArray::from_slice(&key[..16]));
    for block in data.as_chunks_mut::<16>().0 {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
}

/// The fields of the SASL buffer in a screensharingd auth message.
fn sasl_fields(msg: &[u8]) -> Option<Cursor<'_>> {
    let mut c = Cursor::at(msg, SASL_OFFSET);
    let len = c.u32()? as usize;
    Some(Cursor::new(c.take(len)?))
}

/// A `%m` field: `u16 len || bytes`.
fn mpi<'a>(c: &mut Cursor<'a>) -> Option<&'a [u8]> {
    let len = c.u16()? as usize;
    c.take(len)
}

/// A `%o` field: `u8 len || bytes`.
fn octets<'a>(c: &mut Cursor<'a>) -> Option<&'a [u8]> {
    let len = c.u8()? as usize;
    c.take(len)
}

/// A checked SRP challenge. The group is always `GROUP`.
struct Challenge {
    salt: Vec<u8>,
    /// The server public value, in `1..N`.
    b: BigUint,
    iterations: u32,
    /// The options string, which packet 2 echoes.
    options: Vec<u8>,
}

/// Parses `%c control || %m N || %m g || %o salt || %m B || %q iterations ||
/// %s options` and accepts only Apple's group and sane values.
fn parse_challenge(msg: &[u8]) -> Result<Challenge, BadChallenge> {
    let (control, n, g, salt, b, iterations, options) = (|| {
        let mut c = sasl_fields(msg)?;
        Some((c.u8()?, mpi(&mut c)?, mpi(&mut c)?, octets(&mut c)?, mpi(&mut c)?, c.u64()?, mpi(&mut c)?))
    })()
    .ok_or(BadChallenge::Malformed)?;
    if control != 0 {
        return Err(BadChallenge::Malformed);
    }
    let group = &*GROUP;
    if BigUint::from_bytes_be(n) != group.n || BigUint::from_bytes_be(g) != group.g {
        return Err(BadChallenge::Group);
    }
    let b = BigUint::from_bytes_be(b);
    if b.is_zero() || b >= group.n {
        return Err(BadChallenge::PublicValue);
    }
    if salt.is_empty() {
        return Err(BadChallenge::Salt);
    }
    if !(1..=SRP_MAX_ITERATIONS).contains(&iterations) {
        return Err(BadChallenge::Iterations(iterations));
    }
    Ok(Challenge {
        salt: salt.to_vec(),
        b,
        iterations: iterations as u32,
        options: options.to_vec(),
    })
}

/// The server's proof M2 from the final SRP message, `%o M2 || %o sIV ||
/// %s sid || %u ttl`.
fn server_proof(msg: &[u8]) -> Option<&[u8]> {
    octets(&mut sasl_fields(msg)?)
}

/// `v` as a big-endian number of the modulus size.
fn pad(v: &BigUint) -> Vec<u8> {
    let raw = v.to_bytes_be();
    let mut out = vec![0u8; SRP_MODULUS_LEN.saturating_sub(raw.len())];
    out.extend_from_slice(&raw);
    out
}

fn sha512(parts: &[&[u8]]) -> [u8; 64] {
    let mut h = Sha512::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// The private key `x = H(salt || H(":" || PBKDF2(password)))`, with the
/// empty identity of Apple's SASL flow.
fn private_key(salt: &[u8], iterations: u32, password: &str) -> BigUint {
    let mut dk = [0u8; SRP_DK_LEN];
    pbkdf2::pbkdf2_hmac::<Sha512>(password.as_bytes(), salt, iterations, &mut dk);
    let inner = sha512(&[b":", &dk]);
    BigUint::from_bytes_be(&sha512(&[salt, &inner]))
}

struct Proof {
    a_pub: Vec<u8>,
    m1: [u8; 64],
    /// The M2 that a host with the password verifier sends back.
    m2: [u8; 64],
    /// `SHA256(K)[..16]`.
    key: [u8; 16],
}

fn solve_srp(ch: &Challenge, password: &str) -> Proof {
    let Group { n, g, h_ng, k } = &*GROUP;

    let mut a_raw = [0u8; 64];
    OsRng.fill_bytes(&mut a_raw);
    let a = BigUint::from_bytes_be(&a_raw) % (n - 1u32) + 1u32;
    let a_pub = pad(&g.modpow(&a, n));
    let b_pub = pad(&ch.b);

    let u = BigUint::from_bytes_be(&sha512(&[&a_pub, &b_pub]));
    let x = private_key(&ch.salt, ch.iterations, password) % n;

    let kgx = (k * g.modpow(&x, n)) % n;
    let base = ((&ch.b + n) - kgx) % n;
    let s = base.modpow(&(&a + &u * &x), n);
    let k_sess = sha512(&[&pad(&s)]);

    let h_empty = sha512(&[]);
    let m1 = sha512(&[h_ng, &h_empty, &ch.salt, &a_pub, &b_pub, &k_sess]);
    let m2 = sha512(&[&a_pub, &m1, &k_sess]);
    let key = Sha256::digest(k_sess)[..16].try_into().expect("SHA-256 has 32 bytes");
    Proof { a_pub, m1, m2, key }
}

/// SRP-6a against screensharingd. Returns `SHA256(K)[..16]` once the host
/// has proven that it holds the password verifier.
pub fn srp_auth<S: Read + Write>(sock: &mut S, user: &str, password: &str) -> Result<[u8; 16]> {
    let server_key = rsa1_init(sock)?;

    // c2s1: the username rides RSA-encrypted in the "modulus" slot.
    let user_b = user.as_bytes();
    let mut inner = (user_b.len() as u32).to_be_bytes().to_vec();
    inner.extend_from_slice(user_b);
    inner.extend_from_slice(&[0, 0, 0]);
    let mut plain = (inner.len() as u32).to_be_bytes().to_vec();
    plain.extend_from_slice(&inner);
    let enc = server_key.encrypt(&mut OsRng, Pkcs1v15Encrypt, &plain).context("RSA encrypt")?;
    if enc.len() != RSA_BLOCK_LEN {
        bail!("unexpected RSA block size {}", enc.len());
    }
    let mut c2s1 = Vec::with_capacity(654);
    c2s1.extend_from_slice(&650u32.to_be_bytes());
    c2s1.extend_from_slice(&1u16.to_le_bytes());
    c2s1.extend_from_slice(b"RSA1\x00\x02");
    c2s1.extend_from_slice(&(RSA_BLOCK_LEN as u16).to_be_bytes());
    c2s1.extend_from_slice(&enc);
    c2s1.resize(4 + 650, 0);
    sock.write_all(&c2s1)?;

    let msg = read_msg(sock)?;
    let ch = parse_challenge(&msg).with_context(|| format!("unusable SRP challenge ({} bytes)", msg.len()))?;
    tracing::debug!(iterations = ch.iterations, salt = ch.salt.len(), "SRP challenge");

    let proof = solve_srp(&ch, password);

    let mut civ = [0u8; 16];
    OsRng.fill_bytes(&mut civ);
    let mut sd = Vec::new();
    sd.extend_from_slice(&(SRP_MODULUS_LEN as u16).to_be_bytes());
    sd.extend_from_slice(&proof.a_pub);
    sd.push(SRP_HASH_LEN);
    sd.extend_from_slice(&proof.m1);
    sd.extend_from_slice(&(ch.options.len() as u16).to_be_bytes());
    sd.extend_from_slice(&ch.options);
    sd.push(16);
    sd.extend_from_slice(&civ);

    let mut pay = Vec::with_capacity(1076);
    pay.extend_from_slice(&1u16.to_le_bytes());
    pay.extend_from_slice(b"RSA1\x00\x02");
    pay.extend_from_slice(&((sd.len() + 4) as u16).to_be_bytes());
    pay.extend_from_slice(&(sd.len() as u32).to_be_bytes());
    pay.extend_from_slice(&sd);
    if pay.len() > 1076 {
        bail!("SRP proof too large ({} bytes)", pay.len());
    }
    pay.resize(1076, 0);
    let mut msg = 1076u32.to_be_bytes().to_vec();
    msg.extend_from_slice(&pay);
    sock.write_all(&msg)?;

    let reply = read_msg(sock)?;
    let result = read_u32(sock)?;
    if result != 0 {
        return Err(AuthError::Rejected(result).into());
    }
    let m2 = server_proof(&reply).with_context(|| format!("malformed SRP server proof ({} bytes)", reply.len()))?;
    if !bool::from(m2.ct_eq(&proof.m2)) {
        return Err(AuthError::ServerProof.into());
    }
    Ok(proof.key)
}

/// AES-128-ECB encrypts a zero-padded 64-byte field. Used for SessionSelect
/// cmd=2 credentials.
pub fn ecb_field(key: &[u8; 16], value: &str) -> [u8; 64] {
    let mut buf = [0u8; 64];
    let b = value.as_bytes();
    let n = b.len().min(64);
    buf[..n].copy_from_slice(&b[..n]);
    aes_ecb_encrypt(key, &mut buf);
    buf
}

/// AES-128-ECB encrypts a single block in place.
pub fn ecb_block(key: &[u8; 16], block: &mut [u8; 16]) {
    aes_ecb_encrypt(key, block);
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor as IoCursor};
    use std::os::unix::net::UnixStream;
    use std::thread;

    use super::*;

    /// An RSA-2048 public key (DER). The fake host never decrypts with it.
    const HOST_RSA_DER: &str = concat!(
        "30820122300d06092a864886f70d01010105000382010f003082010a0282010100e03dfc269c1ff908f6876959ace87c",
        "30d746cf1d8a6bf22efa2f8fb886814edccb6ddce69714bc94ae6e7237f3e0ff2a3d1a22fb8219a48463b09a7720fe6a",
        "41555bf73f59bf4e693e90b7fc660b3afdc0e5b44d9db8f5bd3894acd2f2d50e59ab76dd4b17ad6e5dc0f7951484e5ed",
        "44c4b69e85e37f82922b5ed89c7144ef79957d0ee27165715809372dd336153aca82394e584f9bd2fa85324921378e0c",
        "532edafb6e4aede04f059ea1d39e5e50a4f0737965c478ce45aa0ff382baf547ce0b4bb7f574edcbdee94fde1b8793e3",
        "2b3c6e5556b3ff9a6c9fd05184a0887a478cd88c1661667aa7516fa150821a13edd7edea3c81eac1b41e022e88c272d5",
        "0b0203010001",
    );
    const OPTIONS: &[u8] = b"mda=SHA-512,replay_detection,conf+int=ChaCha20-Poly1305,kdf=SALTED-SHA512-PBKDF2";
    const SALT: [u8; 32] = [0x5a; 32];
    const ITERATIONS: u64 = 1000;
    /// Bytes the client writes before packet 2: the RSA1 init and c2s1.
    const BEFORE_PROOF: usize = 15 + 4 + 650;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn framed(body: &[u8]) -> Vec<u8> {
        [&(body.len() as u32).to_be_bytes()[..], body].concat()
    }

    fn key_reply() -> Vec<u8> {
        let der = hex(HOST_RSA_DER);
        framed(&[&[0x00, 0x01][..], &(der.len() as u32).to_be_bytes(), &der].concat())
    }

    /// A screensharingd auth message around a SASL buffer of `fields`.
    fn sasl_msg(fields: &[u8]) -> Vec<u8> {
        let len = fields.len() as u32;
        [&[0x00, 0x01][..], &(len + 8).to_be_bytes(), &len.to_be_bytes(), fields].concat()
    }

    fn mpi_field(v: &[u8]) -> Vec<u8> {
        [&(v.len() as u16).to_be_bytes()[..], v].concat()
    }

    fn challenge(n: &BigUint, g: &BigUint, salt: &[u8], b: &BigUint, iterations: u64) -> Vec<u8> {
        let fields = [
            &[0x00][..],
            &mpi_field(&n.to_bytes_be()),
            &mpi_field(&g.to_bytes_be()),
            &[salt.len() as u8],
            salt,
            &mpi_field(&b.to_bytes_be()),
            &iterations.to_be_bytes(),
            &mpi_field(OPTIONS),
        ]
        .concat();
        sasl_msg(&fields)
    }

    fn apple_challenge(b: &BigUint, iterations: u64) -> Vec<u8> {
        challenge(&GROUP.n, &GROUP.g, &SALT, b, iterations)
    }

    /// Canned host bytes in, client bytes out.
    struct Wire {
        input: IoCursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Wire {
        fn new(input: Vec<u8>) -> Self {
            Self { input: IoCursor::new(input), output: Vec::new() }
        }
    }

    impl Read for Wire {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Wire {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Copy)]
    enum Host {
        Honest,
        WrongServerProof,
    }

    /// Plays screensharingd for one login. Returns the host's session key
    /// when it accepted the client's proof.
    fn serve(mut s: UnixStream, password: &str, host: Host) -> io::Result<Option<[u8; 16]>> {
        let Group { n, g, h_ng, k } = &*GROUP;
        read_vec(&mut s, 15)?;
        s.write_all(&key_reply())?;
        read_vec(&mut s, 4 + 650)?;

        let v = g.modpow(&(private_key(&SALT, ITERATIONS as u32, password) % n), n);
        let b = BigUint::from_bytes_be(&[0x42; 64]);
        let b_pub = (k * &v + g.modpow(&b, n)) % n;
        s.write_all(&framed(&apple_challenge(&b_pub, ITERATIONS)))?;

        let proof = read_vec(&mut s, 4 + 1076)?;
        let (a_pub, m1) = (&proof[4 + 16..4 + 528], &proof[4 + 529..4 + 593]);
        let u = BigUint::from_bytes_be(&sha512(&[a_pub, &pad(&b_pub)]));
        let shared = (BigUint::from_bytes_be(a_pub) * v.modpow(&u, n)).modpow(&b, n);
        let k_sess = sha512(&[&pad(&shared)]);
        let expected = sha512(&[h_ng, &sha512(&[]), &SALT, a_pub, &pad(&b_pub), &k_sess]);
        if m1 != expected {
            s.write_all(&framed(&sasl_msg(&[0])))?;
            s.write_all(&1u32.to_be_bytes())?;
            return Ok(None);
        }
        let mut m2 = sha512(&[a_pub, m1, &k_sess]);
        if let Host::WrongServerProof = host {
            m2[0] ^= 1;
        }
        let fields = [&[64][..], &m2, &[16], &[9; 16], &[0, 0], &[0; 4]].concat();
        s.write_all(&framed(&sasl_msg(&fields)))?;
        s.write_all(&0u32.to_be_bytes())?;
        Ok(Some(Sha256::digest(k_sess)[..16].try_into().unwrap()))
    }

    fn login(client_password: &str, host_password: &'static str, host: Host) -> (Result<[u8; 16]>, Option<[u8; 16]>) {
        let (mut client, server) = UnixStream::pair().unwrap();
        let host = thread::spawn(move || serve(server, host_password, host).unwrap());
        let result = srp_auth(&mut client, "alice", client_password);
        // An early client error must end the host's read, not hang the join.
        drop(client);
        (result, host.join().unwrap())
    }

    #[test]
    fn right_password_gives_both_sides_the_same_key() {
        let (client, host) = login("hunter2", "hunter2", Host::Honest);
        let key = client.unwrap();
        assert_eq!(Some(key), host);
    }

    #[test]
    fn wrong_password_is_rejected_by_the_host() {
        let (client, host) = login("hunter3", "hunter2", Host::Honest);
        let e = client.unwrap_err();
        assert!(matches!(e.downcast_ref::<AuthError>(), Some(AuthError::Rejected(1))), "{e:#}");
        assert_eq!(host, None);
    }

    #[test]
    fn host_with_a_wrong_server_proof_is_refused() {
        let (client, host) = login("hunter2", "hunter2", Host::WrongServerProof);
        let e = client.unwrap_err();
        assert!(matches!(e.downcast_ref::<AuthError>(), Some(AuthError::ServerProof)), "{e:#}");
        assert!(host.is_some(), "the host accepted M1, only M2 was wrong");
    }

    #[test]
    fn generator_1_host_gets_no_proof() {
        // With g = 1 and B = k + 1, S is 1 for every password, so the host
        // would know the session key without knowing the password.
        let one = BigUint::from(1u32);
        let k1 = BigUint::from_bytes_be(&sha512(&[&pad(&GROUP.n), &pad(&one)])) % &GROUP.n;
        let mut wire = Wire::new([key_reply(), framed(&challenge(&GROUP.n, &one, &SALT, &(k1 + 1u32), ITERATIONS))].concat());
        let e = srp_auth(&mut wire, "alice", "hunter2").unwrap_err();
        assert_eq!(e.downcast_ref::<BadChallenge>(), Some(&BadChallenge::Group), "{e:#}");
        assert_eq!(wire.output.len(), BEFORE_PROOF, "no packet 2 with M1");
    }

    #[test]
    fn parses_apples_1165_byte_challenge() {
        let b = &GROUP.n - 12345u32;
        let msg = apple_challenge(&b, 166_666);
        // The reference client reads N at offset 13 after a zero byte.
        assert_eq!(msg.len(), 1165);
        assert_eq!(msg[12], 0);
        assert_eq!(msg[13..525], pad(&GROUP.n));
        let ch = parse_challenge(&msg).unwrap();
        assert_eq!((ch.salt.as_slice(), ch.iterations, ch.options.as_slice()), (&SALT[..], 166_666, OPTIONS));
        assert_eq!(ch.b, b);
    }

    #[test]
    fn refuses_any_other_group() {
        let b = BigUint::from(7u32);
        let two = BigUint::from(2u32);
        let other_n = &GROUP.n - 2u32;
        assert_eq!(parse_challenge(&challenge(&GROUP.n, &two, &SALT, &b, ITERATIONS)).err(), Some(BadChallenge::Group));
        assert_eq!(parse_challenge(&challenge(&other_n, &GROUP.g, &SALT, &b, ITERATIONS)).err(), Some(BadChallenge::Group));
        let zero = BigUint::from(0u32);
        assert_eq!(parse_challenge(&challenge(&zero, &GROUP.g, &SALT, &b, ITERATIONS)).err(), Some(BadChallenge::Group));
    }

    #[test]
    fn refuses_b_of_0_or_a_multiple_of_n() {
        for b in [BigUint::from(0u32), GROUP.n.clone(), &GROUP.n * 2u32] {
            assert_eq!(parse_challenge(&apple_challenge(&b, ITERATIONS)).err(), Some(BadChallenge::PublicValue));
        }
    }

    #[test]
    fn refuses_an_empty_salt_and_iteration_counts_out_of_range() {
        let b = BigUint::from(7u32);
        assert_eq!(parse_challenge(&challenge(&GROUP.n, &GROUP.g, &[], &b, ITERATIONS)).err(), Some(BadChallenge::Salt));
        for n in [0, 1_000_001, u64::MAX] {
            assert_eq!(parse_challenge(&apple_challenge(&b, n)).err(), Some(BadChallenge::Iterations(n)));
        }
    }

    #[test]
    fn truncated_or_marked_challenge_is_malformed() {
        let msg = apple_challenge(&BigUint::from(7u32), ITERATIONS);
        for len in 0..msg.len() {
            assert_eq!(parse_challenge(&msg[..len]).err(), Some(BadChallenge::Malformed), "cut at {len}");
        }
        let mut marked = msg.clone();
        marked[10] = 1; // the control byte
        assert_eq!(parse_challenge(&marked).err(), Some(BadChallenge::Malformed));
    }

    #[test]
    fn huge_length_fields_fail_without_allocating() {
        let mut wire = Wire::new(u32::MAX.to_be_bytes().to_vec());
        let e = srp_auth(&mut wire, "alice", "hunter2").unwrap_err();
        assert!(e.to_string().contains("larger than 8192"), "{e:#}");

        let mut wire = Wire::new([key_reply(), u32::MAX.to_be_bytes().to_vec()].concat());
        let e = srp_auth(&mut wire, "alice", "hunter2").unwrap_err();
        assert!(e.to_string().contains("larger than 8192"), "{e:#}");

        // An inner length past the end of the message.
        let mut wire = Wire::new(framed(&[0x00, 0x01, 0xff, 0xff, 0xff, 0xff]));
        assert!(srp_auth(&mut wire, "alice", "hunter2").is_err());
    }

    #[test]
    fn server_proof_is_the_first_octet_field() {
        let fields = [&[3][..], b"abc", &[16], &[9; 16], &[0, 0], &[0; 4]].concat();
        assert_eq!(server_proof(&sasl_msg(&fields)), Some(&b"abc"[..]));
        assert_eq!(server_proof(&sasl_msg(&[64, 1, 2])), None);
    }

    #[test]
    fn modulus_is_the_rfc_3526_prime() {
        // N = 2^4096 - 2^4032 - 1 + 2^64 * (floor(2^3966 * pi) + 240904), with
        // pi = 16 atan(1/5) - 4 atan(1/239) and 64 guard bits.
        fn atan_inv(x: u32, one: &BigUint) -> (BigUint, BigUint) {
            let (mut plus, mut minus) = (BigUint::zero(), BigUint::zero());
            let mut term = one / x;
            for i in 0u32.. {
                if term.is_zero() {
                    break;
                }
                let t = &term / (2 * i + 1);
                if i % 2 == 0 { plus += t } else { minus += t }
                term /= x * x;
            }
            (plus, minus)
        }
        let one = BigUint::from(1u32) << (3966 + 64);
        let (p5, m5) = atan_inv(5, &one);
        let (p239, m239) = atan_inv(239, &one);
        let pi = ((p5 * 16u32 + m239 * 4u32) - (m5 * 16u32 + p239 * 4u32)) >> 64;
        let n = (BigUint::from(1u32) << 4096) - (BigUint::from(1u32) << 4032) - 1u32 + ((pi + 240_904u32) << 64);
        assert_eq!(GROUP.n, n);
    }
}
