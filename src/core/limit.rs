//! Anti-spam : chaque membre a un petit crédit de messages qui se recharge
//! avec le temps. Le client s'y tient (il prévient au lieu d'envoyer) et
//! l'hôte l'applique aussi, un peu plus large, à ceux qui ne s'y tiendraient
//! pas.

use std::time::{Duration, Instant};

/// Seau à jetons : `capacity` d'un coup, puis `per_sec` par seconde.
#[derive(Debug, Clone)]
pub struct Bucket {
    capacity: f32,
    per_sec: f32,
    tokens: f32,
    last: Instant,
}

impl Bucket {
    pub fn new(capacity: f32, per_sec: f32) -> Self {
        Self {
            capacity,
            per_sec,
            tokens: capacity,
            last: Instant::now(),
        }
    }

    /// Messages texte : 5 d'affilée, puis un peu moins d'un par seconde.
    pub fn messages() -> Self {
        Self::new(5.0, 0.8)
    }

    /// Réactions, modifications, suppressions.
    pub fn actions() -> Self {
        Self::new(20.0, 4.0)
    }

    /// Même rythme, avec de la marge (côté hôte : les horloges et le réseau
    /// ne tombent jamais pile).
    pub fn lenient(mut self) -> Self {
        self.capacity += 3.0;
        self.tokens = self.capacity;
        self.per_sec *= 1.5;
        self
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f32();
        self.tokens = (self.tokens + elapsed * self.per_sec).min(self.capacity);
        self.last = now;
    }

    /// Prend un jeton s'il en reste.
    pub fn take(&mut self) -> bool {
        self.take_at(Instant::now())
    }

    pub fn take_at(&mut self, now: Instant) -> bool {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Temps avant le prochain jeton.
    pub fn wait(&self) -> Duration {
        let missing = (1.0 - self.tokens).max(0.0);
        Duration::from_secs_f32(missing / self.per_sec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rafale_puis_rythme() {
        let t0 = Instant::now();
        let mut b = Bucket::new(3.0, 1.0);
        assert!((0..3).all(|_| b.take_at(t0)));
        assert!(!b.take_at(t0));
        assert!(!b.take_at(t0 + Duration::from_millis(500)));
        assert!(b.take_at(t0 + Duration::from_millis(1100)));
        // Le crédit ne dépasse pas la capacité, même après une longue pause.
        let later = t0 + Duration::from_secs(60);
        assert!((0..3).all(|_| b.take_at(later)));
        assert!(!b.take_at(later));
    }
}
