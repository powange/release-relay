//! Limite de requêtes par adresse : seau de `per_minute` jetons rempli au fil de la minute
//! (une rafale de `per_minute` requêtes passe, puis une requête toutes les 60/`per_minute` s).
//! Au-delà : 429 avec `Retry-After`.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;

/// Adresses suivies au plus (au-delà, celles dont le seau est plein sont oubliées, puis toutes)
const MAX_TRACKED: usize = 10_000;

struct Bucket {
    tokens: f64,
    at: Instant,
}

pub struct Limiter {
    per_minute: u32,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

impl Limiter {
    /// `per_minute` à 0 : sans limite
    pub fn new(per_minute: u32) -> Limiter {
        Limiter { per_minute, buckets: Mutex::new(HashMap::new()) }
    }

    fn refill(&self, b: &mut Bucket, now: Instant) {
        let rate = f64::from(self.per_minute) / 60.0;
        b.tokens = (b.tokens + now.saturating_duration_since(b.at).as_secs_f64() * rate).min(f64::from(self.per_minute));
        b.at = now;
    }

    /// Requête acceptée, ou temps d'attente avant la prochaine
    pub fn check(&self, ip: IpAddr, now: Instant) -> Result<(), Duration> {
        if self.per_minute == 0 {
            return Ok(());
        }
        let key = limit_key(ip);
        let mut buckets = self.buckets.lock().unwrap();
        if buckets.len() >= MAX_TRACKED && !buckets.contains_key(&key) {
            let full = f64::from(self.per_minute);
            buckets.retain(|_, b| {
                let elapsed = now.saturating_duration_since(b.at).as_secs_f64();
                b.tokens + elapsed * full / 60.0 < full
            });
            if buckets.len() >= MAX_TRACKED {
                buckets.clear();
            }
        }
        let b = buckets.entry(key).or_insert(Bucket { tokens: f64::from(self.per_minute), at: now });
        self.refill(b, now);
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            let rate = f64::from(self.per_minute) / 60.0;
            Err(Duration::from_secs_f64((1.0 - b.tokens) / rate))
        }
    }
}

/// Adresse comptée : une adresse IPv6 vaut tout son réseau /64 (un abonné en reçoit des
/// milliards), une IPv4 écrite en IPv6 vaut l'IPv4
fn limit_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & !((1u128 << 64) - 1))),
        },
        v4 => v4,
    }
}

/// Adresse du client : derrière `hops` proxys, celle ajoutée par le premier d'entre eux à
/// `X-Forwarded-For` (en partant de la droite : les adresses plus à gauche viennent du client
/// et ne prouvent rien) ; sinon celle de la connexion
pub fn client_ip(headers: &HeaderMap, peer: Option<IpAddr>, hops: usize) -> IpAddr {
    let peer = peer.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    if hops == 0 {
        return peer;
    }
    let forwarded: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .collect();
    // Une fois : combien d'adresses le ou les proxys ajoutent, pour vérifier proxy_hops
    static LOGGED: std::sync::Once = std::sync::Once::new();
    LOGGED.call_once(|| {
        tracing::info!(
            "X-Forwarded-For : {} adresse(s) reçue(s), proxy_hops = {hops} (Cloudflare devant le proxy : mettre 2)",
            forwarded.len()
        );
    });
    if forwarded.is_empty() {
        return peer;
    }
    let pick = forwarded[forwarded.len().saturating_sub(hops)];
    pick.parse().unwrap_or(peer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_wait() {
        let l = Limiter::new(60);
        let ip: IpAddr = "203.0.113.1".parse().unwrap();
        let t = Instant::now();
        for _ in 0..60 {
            assert!(l.check(ip, t).is_ok());
        }
        let wait = l.check(ip, t).unwrap_err();
        assert!(wait <= Duration::from_secs(1));
        // Une autre adresse n'est pas touchée
        assert!(l.check("203.0.113.2".parse().unwrap(), t).is_ok());
        // Une seconde plus tard : un jeton de plus
        assert!(l.check(ip, t + Duration::from_secs(1)).is_ok());
        assert!(l.check(ip, t + Duration::from_secs(1)).is_err());
    }

    #[test]
    fn zero_means_no_limit() {
        let l = Limiter::new(0);
        let t = Instant::now();
        for _ in 0..1000 {
            assert!(l.check("203.0.113.1".parse().unwrap(), t).is_ok());
        }
    }

    #[test]
    fn ipv6_counted_by_network() {
        let l = Limiter::new(2);
        let t = Instant::now();
        assert!(l.check("2001:db8::1".parse().unwrap(), t).is_ok());
        assert!(l.check("2001:db8::2".parse().unwrap(), t).is_ok());
        assert!(l.check("2001:db8::3".parse().unwrap(), t).is_err());
        assert!(l.check("2001:db8:0:1::1".parse().unwrap(), t).is_ok());
    }

    #[test]
    fn client_address() {
        let mut h = HeaderMap::new();
        let peer = Some("10.0.0.2".parse().unwrap());
        assert_eq!(client_ip(&h, peer, 1), "10.0.0.2".parse::<IpAddr>().unwrap());
        h.insert("x-forwarded-for", "1.1.1.1, 203.0.113.9".parse().unwrap());
        assert_eq!(client_ip(&h, peer, 1), "203.0.113.9".parse::<IpAddr>().unwrap());
        assert_eq!(client_ip(&h, peer, 2), "1.1.1.1".parse::<IpAddr>().unwrap());
        assert_eq!(client_ip(&h, peer, 0), "10.0.0.2".parse::<IpAddr>().unwrap());
    }
}
