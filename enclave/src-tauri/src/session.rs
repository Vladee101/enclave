use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use uuid::Uuid;

use crate::error::AppError;

/// The signed-in user, as the core knows it.
///
/// Commands take the caller's identity from here, never from IPC arguments:
/// a `user_id` passed by the webview is only a claim, and RLS (ADR-0008)
/// faithfully enforces whatever identity it is handed. `cmd_login` is the
/// only writer, after verifying the PIN. One session per window; the
/// office server keeps one per token (`office::server`, ADR-0031).
#[derive(Default)]
pub struct Session(Mutex<Option<SessionUser>>);

#[derive(Serialize, serde::Deserialize, Clone, Debug)]
pub struct SessionUser {
    pub id:       Uuid,
    pub username: String,
    /// Display hint for the UI only. Admin-only commands re-check
    /// `users.is_admin` in the database on every call (`require_admin`),
    /// so a demotion takes effect without a re-login.
    pub is_admin: bool,
}

impl Session {
    pub fn set(&self, user: SessionUser) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(user);
    }

    pub fn clear(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn get(&self) -> Option<SessionUser> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The window's user as the caller of a command.
    pub fn caller(&self) -> Caller {
        Caller(self.get())
    }
}

/// Who a command runs for: the window's session, or the session behind an
/// office client's token. The core functions under `commands` take this,
/// so the same function serves both, and neither can be told who the
/// caller is by the caller.
#[derive(Clone, Debug, Default)]
pub struct Caller(pub Option<SessionUser>);

impl Caller {
    /// The signed-in user, or the error every user-scoped command returns
    /// when nobody is.
    pub fn require(&self) -> Result<SessionUser, AppError> {
        self.0.clone().ok_or_else(|| AppError::new("not_signed_in", "Not signed in."))
    }

    pub fn get(&self) -> Option<&SessionUser> {
        self.0.as_ref()
    }
}

/// Failed PIN attempts, per profile and per network address (ADR-0031).
/// A PIN is short; over the network attempts would otherwise be free.
/// After `FREE_FAILURES`, each attempt waits out a pause that doubles with
/// every further failure, up to `MAX_PAUSE`. A success clears both keys.
/// In memory: a restart forgets, which costs an attacker a restart they
/// cannot cause.
#[derive(Default)]
pub struct LoginGuard(Mutex<HashMap<String, Failures>>);

#[derive(Clone, Copy)]
struct Failures {
    count: u32,
    last:  Instant,
}

pub const FREE_FAILURES: u32 = 5;
const FIRST_PAUSE: Duration = Duration::from_secs(30);
const MAX_PAUSE: Duration = Duration::from_secs(15 * 60);

impl LoginGuard {
    fn pause(count: u32) -> Duration {
        if count < FREE_FAILURES {
            return Duration::ZERO;
        }
        let doubled = FIRST_PAUSE.saturating_mul(1u32 << (count - FREE_FAILURES).min(10));
        doubled.min(MAX_PAUSE)
    }

    /// Whether `keys` may try now; if not, the error to return, with the
    /// seconds left. Checked before the PIN, so a throttled attempt never
    /// learns whether the PIN was right.
    pub fn check(&self, keys: &[String], now: Instant) -> Result<(), AppError> {
        let map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let wait = keys
            .iter()
            .filter_map(|k| map.get(k))
            .map(|f| Self::pause(f.count).saturating_sub(now.duration_since(f.last)))
            .max()
            .unwrap_or_default();
        if wait.is_zero() {
            Ok(())
        } else {
            let seconds = wait.as_secs().max(1);
            Err(AppError::new("login_throttled", format!("Too many wrong PINs. Try again in {seconds} s."))
                .with("seconds", seconds))
        }
    }

    pub fn failed(&self, keys: &[String], now: Instant) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        for k in keys {
            let f = map.entry(k.clone()).or_insert(Failures { count: 0, last: now });
            f.count += 1;
            f.last = now;
        }
    }

    pub fn succeeded(&self, keys: &[String]) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        for k in keys {
            map.remove(k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_wrong_pins_are_free_then_the_pause_doubles() {
        let guard = LoginGuard::default();
        let keys = vec!["user:a".to_string(), "addr:10.0.0.7".to_string()];
        let t0 = Instant::now();
        for _ in 0..FREE_FAILURES {
            guard.check(&keys, t0).unwrap();
            guard.failed(&keys, t0);
        }
        let err = guard.check(&keys, t0).unwrap_err();
        assert_eq!((err.code, err.params["seconds"].as_u64()), ("login_throttled", Some(30)));
        guard.check(&keys, t0 + Duration::from_secs(30)).unwrap();
        guard.failed(&keys, t0 + Duration::from_secs(30));
        assert_eq!(guard.check(&keys, t0 + Duration::from_secs(31)).unwrap_err().params["seconds"].as_u64(), Some(59));

        // Another profile from the same address is paused too; a success
        // clears the keys it was checked with.
        let other = vec!["user:b".to_string(), "addr:10.0.0.7".to_string()];
        assert!(guard.check(&other, t0 + Duration::from_secs(31)).is_err());
        guard.succeeded(&keys);
        guard.check(&other, t0 + Duration::from_secs(31)).unwrap();
    }

    #[test]
    fn the_pause_stops_growing() {
        assert_eq!(LoginGuard::pause(4), Duration::ZERO);
        assert_eq!(LoginGuard::pause(5), FIRST_PAUSE);
        assert_eq!(LoginGuard::pause(40), MAX_PAUSE);
    }
}
