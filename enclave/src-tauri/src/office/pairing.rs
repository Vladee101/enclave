//! Connecting a computer to the office server by a one-time code
//! (ADR-0031).
//!
//! The administrator makes an invitation on the server: a code of 16
//! base32 characters (80 bits), good for 10 minutes and one computer. On
//! the client the user types the server's address and the code. The code
//! itself never crosses the network — it is the key of two HMACs over the
//! server certificate's fingerprint and a nonce the client picks:
//!
//! 1. `pair/proof`: the server shows it knows the code for *its own*
//!    certificate. A machine in the middle presents another certificate,
//!    and cannot compute the proof for it without the code; relaying the
//!    real server's proof does not help, since the client checks it
//!    against the certificate it actually got.
//! 2. `pair/confirm`: the client shows it knows the code; the server burns
//!    the invitation. Five wrong confirmations burn it too.
//!
//! The client then pins that certificate. Pairing grants nothing on the
//! server — signing in still takes a profile and its PIN.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::error::AppError;

pub const LIFETIME: Duration = Duration::from_secs(10 * 60);
pub const MAX_WRONG: u32 = 5;
const SERVER_LABEL: &[u8] = b"enclave-pair-server";
const CLIENT_LABEL: &[u8] = b"enclave-pair-client";
const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// The live invitation, if any. One at a time: a new one replaces it.
#[derive(Default)]
pub struct Invitations(Mutex<Option<Invite>>);

struct Invite {
    code:       String,
    created_by: Uuid,
    expires:    Instant,
    wrong:      u32,
}

impl Invitations {
    /// A new code (normalised, 16 characters), replacing any earlier one.
    pub fn create(&self, created_by: Uuid, now: Instant) -> String {
        let code = generate_code();
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(Invite { code: code.clone(), created_by, expires: now + LIFETIME, wrong: 0 });
        code
    }

    fn live<T>(&self, now: Instant, f: impl FnOnce(&mut Option<Invite>) -> Result<T, AppError>) -> Result<T, AppError> {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(|i| now >= i.expires) {
            *slot = None;
        }
        if slot.is_none() {
            return Err(no_invitation());
        }
        f(&mut slot)
    }

    /// Step 1: the server's proof for its certificate and the client's nonce.
    pub fn proof(&self, fingerprint: &str, nonce: &str, now: Instant) -> Result<String, AppError> {
        self.live(now, |slot| {
            let invite = slot.as_ref().expect("live");
            Ok(hex(&mac(&invite.code, SERVER_LABEL, fingerprint, nonce)))
        })
    }

    /// Step 2: the client's proof. Right — the invitation is used up, and
    /// the administrator who made it is returned for the audit. Wrong —
    /// counted, and the fifth burns the invitation.
    pub fn confirm(&self, fingerprint: &str, nonce: &str, client_mac: &str, now: Instant) -> Result<Uuid, AppError> {
        self.live(now, |slot| {
            let invite = slot.as_mut().expect("live");
            let ok = unhex(client_mac).is_some_and(|m| verify(&invite.code, CLIENT_LABEL, fingerprint, nonce, &m));
            if ok {
                let by = invite.created_by;
                *slot = None;
                return Ok(by);
            }
            invite.wrong += 1;
            if invite.wrong >= MAX_WRONG {
                *slot = None;
            }
            Err(pairing_failed())
        })
    }
}

/// What the client computes for step 1, to compare with the server's.
pub fn server_proof_matches(code: &str, fingerprint: &str, nonce: &str, proof: &str) -> bool {
    unhex(proof).is_some_and(|m| verify(code, SERVER_LABEL, fingerprint, nonce, &m))
}

/// The client's proof for step 2.
pub fn client_proof(code: &str, fingerprint: &str, nonce: &str) -> String {
    hex(&mac(code, CLIENT_LABEL, fingerprint, nonce))
}

pub fn no_invitation() -> AppError {
    AppError::new(
        "invitation_expired",
        "The server has no valid code: it expired, was used, or a newer one replaced it. Make a new one on the server.",
    )
}

pub fn pairing_failed() -> AppError {
    AppError::new(
        "pairing_failed",
        "The code does not match. Check it — or the computer at this address is not your Enclave server.",
    )
}

fn hmac_of(code: &str, label: &[u8], fingerprint: &str, nonce: &str) -> Hmac<Sha256> {
    let mut m = Hmac::<Sha256>::new_from_slice(code.as_bytes()).expect("HMAC takes any key length");
    for part in [label, fingerprint.as_bytes(), nonce.as_bytes()] {
        // Length-prefixed, so no two different inputs read the same.
        m.update(&(part.len() as u32).to_be_bytes());
        m.update(part);
    }
    m
}

fn mac(code: &str, label: &[u8], fingerprint: &str, nonce: &str) -> Vec<u8> {
    hmac_of(code, label, fingerprint, nonce).finalize().into_bytes().to_vec()
}

/// Constant-time comparison.
fn verify(code: &str, label: &[u8], fingerprint: &str, nonce: &str, given: &[u8]) -> bool {
    hmac_of(code, label, fingerprint, nonce).verify_slice(given).is_ok()
}

pub fn generate_code() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the OS random source");
    // 16 characters × 5 bits = 80 bits.
    bytes.iter().map(|b| ALPHABET[(*b & 31) as usize] as char).collect()
}

/// As typed: any case, with or without dashes and spaces. The letters a
/// person confuses with digits are read as the letters the alphabet has
/// (no 0, 1, 8 or 9 in base32).
pub fn normalize(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c.to_ascii_uppercase() {
            '0' => 'O',
            '1' => 'I',
            '8' => 'B',
            other => other,
        })
        .collect()
}

/// `ABCDEFGHIJKLMNOP` → `ABCD-EFGH-IJKL-MNOP`.
pub fn display(code: &str) -> String {
    code.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join("-")
}

pub fn random_nonce() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the OS random source");
    hex(&bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_16_base32_characters_and_read_back_as_typed() {
        let code = generate_code();
        assert_eq!(code.len(), 16);
        assert!(code.bytes().all(|b| ALPHABET.contains(&b)));
        assert_eq!(normalize(&display(&code).to_lowercase()), code);
        assert_eq!(normalize(" abcd-ef0h 1jkl-mn8p "), "ABCDEFOHIJKLMNBP");
        assert_eq!(display("ABCDEFGHIJKLMNOP"), "ABCD-EFGH-IJKL-MNOP");
    }

    #[test]
    fn the_proofs_bind_the_code_to_one_certificate() {
        let invitations = Invitations::default();
        let t0 = Instant::now();
        let admin = Uuid::new_v4();
        let code = invitations.create(admin, t0);
        let nonce = random_nonce();

        let proof = invitations.proof("server-fp", &nonce, t0).unwrap();
        assert!(server_proof_matches(&code, "server-fp", &nonce, &proof));
        // The same proof for a certificate in the middle does not match,
        // and neither does a guess at the code.
        assert!(!server_proof_matches(&code, "attacker-fp", &nonce, &proof));
        assert!(!server_proof_matches(&generate_code(), "server-fp", &nonce, &proof));

        // A wrong client proof is counted; the right one uses the code up.
        let e = invitations.confirm("server-fp", &nonce, &client_proof(&generate_code(), "server-fp", &nonce), t0).unwrap_err();
        assert_eq!(e.code, "pairing_failed");
        assert_eq!(invitations.confirm("server-fp", &nonce, &client_proof(&code, "server-fp", &nonce), t0).unwrap(), admin);
        let e = invitations.confirm("server-fp", &nonce, &client_proof(&code, "server-fp", &nonce), t0).unwrap_err();
        assert_eq!(e.code, "invitation_expired");
    }

    #[test]
    fn an_invitation_ends_after_ten_minutes_or_five_wrong_codes() {
        let invitations = Invitations::default();
        let t0 = Instant::now();
        invitations.create(Uuid::new_v4(), t0);
        assert!(invitations.proof("fp", "n", t0 + LIFETIME - Duration::from_secs(1)).is_ok());
        assert_eq!(invitations.proof("fp", "n", t0 + LIFETIME).unwrap_err().code, "invitation_expired");

        let code = invitations.create(Uuid::new_v4(), t0);
        for _ in 0..MAX_WRONG {
            assert_eq!(invitations.confirm("fp", "n", "00", t0).unwrap_err().code, "pairing_failed");
        }
        let right = client_proof(&code, "fp", "n");
        assert_eq!(invitations.confirm("fp", "n", &right, t0).unwrap_err().code, "invitation_expired");
    }
}
