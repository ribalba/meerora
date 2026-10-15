//! A simple per-address rate limit for the requests that create something:
//! accounts, manifest registrations and posts. Each client (an IPv4 address
//! or an IPv6 /64, see [`crate::http::client_key`]) gets a fixed number of
//! requests per action and hour. Requests without a peer address (tests
//! that call the handler directly) are not limited.
//!
//! Behind a reverse proxy every request comes from the proxy's address, so
//! a public index needs the client address from the proxy first (Q10).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;

use crate::http::client_key;

/// One hour, the window every limit counts in.
pub const WINDOW_MS: u64 = 3_600_000;
/// The most clients the limiter remembers before it drops old windows.
const MAX_CLIENTS: usize = 100_000;

/// What a client does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    CreateAccount,
    Register,
    Post,
}

impl Action {
    /// Requests per hour from one client.
    pub fn per_hour(self) -> u32 {
        match self {
            Action::CreateAccount => 10,
            Action::Register => 60,
            Action::Post => 120,
        }
    }

    fn what(self) -> &'static str {
        match self {
            Action::CreateAccount => "new accounts",
            Action::Register => "manifest uploads",
            Action::Post => "posts",
        }
    }
}

#[derive(Default)]
pub struct RateLimiter {
    /// (client, action) to (start of the window, requests in it).
    windows: Mutex<HashMap<(IpAddr, Action), (u64, u32)>>,
}

impl RateLimiter {
    /// Counts one request of `ip` at `now` (Unix milliseconds). Refuses with
    /// a message when the client used up its hour.
    pub fn check(&self, ip: Option<IpAddr>, action: Action, now: u64) -> Result<(), String> {
        let Some(ip) = ip else { return Ok(()) };
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        if windows.len() >= MAX_CLIENTS {
            windows.retain(|_, (start, _)| now.saturating_sub(*start) < WINDOW_MS);
        }
        let (start, count) = windows.entry((client_key(ip), action)).or_insert((now, 0));
        if now.saturating_sub(*start) >= WINDOW_MS {
            *start = now;
            *count = 0;
        }
        if *count >= action.per_hour() {
            let minutes = (WINDOW_MS - now.saturating_sub(*start)).div_ceil(60_000);
            return Err(format!(
                "too many {} from your address, at most {} an hour. Try again in {minutes} minutes.",
                action.what(),
                action.per_hour()
            ));
        }
        *count += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_client_has_its_hour() {
        let limiter = RateLimiter::default();
        let a: IpAddr = "203.0.113.7".parse().unwrap();
        let b: IpAddr = "198.51.100.9".parse().unwrap();
        for _ in 0..Action::CreateAccount.per_hour() {
            limiter.check(Some(a), Action::CreateAccount, 1_000).unwrap();
        }
        let err = limiter.check(Some(a), Action::CreateAccount, 2_000).unwrap_err();
        assert!(err.contains("too many new accounts") && err.contains("60 minutes"), "{err}");
        assert!(limiter.check(Some(b), Action::CreateAccount, 2_000).is_ok(), "another address has its own count");
        assert!(limiter.check(Some(a), Action::Post, 2_000).is_ok(), "and so has another action");
        assert!(limiter.check(None, Action::CreateAccount, 2_000).is_ok());
        assert!(limiter.check(Some(a), Action::CreateAccount, 1_000 + WINDOW_MS).is_ok(), "the next hour starts afresh");
    }
}
