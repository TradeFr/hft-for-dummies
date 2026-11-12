//! Markouts: was each fill a good trade?
//!
//! The profit number alone can't tell you *why* you make or lose money. So
//! after every fill we keep watching the fair value for a minute:
//!
//! - **Spread captured**: how far from fair we traded, at the moment of the
//!   fill. Buying at 99.90 when fair is 100.00 captures +10 bps.
//! - **Markout at 1 s, 5 s, 30 s, 60 s**: how far from fair our price is that
//!   much later. If fair is 100.20 a second after we bought at 99.90, the
//!   trade is +30 bps. If fair dropped to 99.70, it is -20 bps: whoever sold
//!   to us knew the price was about to fall.
//! - **Adverse selection** = markout - spread captured: what the market took
//!   back after the fill. Market makers live or die by this number.
//!
//! This is a plain state machine like the market maker: no clock, no I/O.
//! It is told the time, and returns what it measured.

use std::collections::VecDeque;
use std::fmt;
use std::time::{Duration, Instant};

use rust_decimal::Decimal;

use crate::events::Side;

/// When to measure each fill after it happens.
pub const HORIZONS: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(60),
];

/// If the first fair value after a horizon arrives later than this (Binance
/// went quiet), we skip that horizon rather than measure at the wrong time.
const LATE: Duration = Duration::from_secs(1);

/// What we know about one horizon of one fill.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Slot {
    Waiting,
    Scored(Decimal),
    Missed,
}

/// A fill we are still watching.
#[derive(Debug, Clone)]
struct Watched {
    at: Instant,
    side: Side,
    price: Decimal,
    size: Decimal,
    captured_bps: Decimal,
    slots: [Slot; 4],
}

/// One fill, fully measured. Logged as a `MARKOUT:` line.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub captured_bps: Decimal,
    /// One per horizon; `None` when that horizon was missed.
    pub markout_bps: [Option<Decimal>; 4],
}

/// A running total for one number: how many, and their sum.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Total {
    count: u64,
    sum_bps: Decimal,
    sum_usd: Decimal,
}

impl Total {
    fn add(&mut self, bps: Decimal, usd: Decimal) {
        self.count += 1;
        self.sum_bps += bps;
        self.sum_usd += usd;
    }

    fn mean_bps(&self) -> Option<Decimal> {
        (self.count > 0).then(|| self.sum_bps / Decimal::from(self.count))
    }
}

#[derive(Debug, Default)]
pub struct Markouts {
    watching: VecDeque<Watched>,
    captured: Total,
    markouts: [Total; 4],
    /// Fills we couldn't measure: there was no fair value when they happened.
    unscored: u64,
}

impl Markouts {
    /// A fill happened. `fair` is our fair value at that moment, if we had one.
    pub fn on_fill(
        &mut self,
        side: Side,
        price: Decimal,
        size: Decimal,
        fair: Option<Decimal>,
        now: Instant,
    ) {
        let Some(fair) = fair else {
            self.unscored += 1;
            return;
        };
        let captured_bps = edge_bps(side, price, fair);
        self.captured
            .add(captured_bps, edge_usd(side, price, size, fair));
        self.watching.push_back(Watched {
            at: now,
            side,
            price,
            size,
            captured_bps,
            slots: [Slot::Waiting; 4],
        });
    }

    /// A new fair value. Scores every horizon that is due, and returns the
    /// fills that are now fully measured.
    pub fn on_fair(&mut self, fair: Decimal, now: Instant) -> Vec<Report> {
        for watched in &mut self.watching {
            for (i, horizon) in HORIZONS.iter().enumerate() {
                if watched.slots[i] != Slot::Waiting {
                    continue;
                }
                let due = watched.at + *horizon;
                if now < due {
                    continue;
                }
                if now.duration_since(due) > LATE {
                    watched.slots[i] = Slot::Missed;
                    continue;
                }
                let bps = edge_bps(watched.side, watched.price, fair);
                let usd = edge_usd(watched.side, watched.price, watched.size, fair);
                watched.slots[i] = Slot::Scored(bps);
                self.markouts[i].add(bps, usd);
            }
        }

        // Fills are watched in time order, so finished ones are at the front.
        let mut done = Vec::new();
        while let Some(front) = self.watching.front() {
            if front.slots.contains(&Slot::Waiting) {
                break;
            }
            let w = self.watching.pop_front().expect("front exists");
            done.push(Report {
                side: w.side,
                price: w.price,
                size: w.size,
                captured_bps: w.captured_bps,
                markout_bps: w.slots.map(|s| match s {
                    Slot::Scored(bps) => Some(bps),
                    _ => None,
                }),
            });
        }
        done
    }

    /// Averages so far, or `None` before the first measured fill.
    pub fn summary(&self) -> Option<Summary> {
        let captured_bps = self.captured.mean_bps()?;
        let markout_bps = self.markouts.map(|t| t.mean_bps());
        // Adverse selection at 5 s: the markout minus what we captured.
        let adverse_5s_bps = markout_bps[1].map(|m| m - captured_bps);
        Some(Summary {
            fills: self.captured.count,
            unscored: self.unscored,
            captured_bps,
            captured_usd: self.captured.sum_usd,
            markout_bps,
            markout_60s_usd: self.markouts[3].sum_usd,
            adverse_5s_bps,
        })
    }
}

/// The averages, logged as a `markouts:` line.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub fills: u64,
    pub unscored: u64,
    pub captured_bps: Decimal,
    pub captured_usd: Decimal,
    pub markout_bps: [Option<Decimal>; 4],
    pub markout_60s_usd: Decimal,
    pub adverse_5s_bps: Option<Decimal>,
}

/// How much better than fair we traded, in basis points. Positive is good:
/// we bought below fair or sold above it.
fn edge_bps(side: Side, price: Decimal, fair: Decimal) -> Decimal {
    if price.is_zero() {
        return Decimal::ZERO;
    }
    sign(side) * (fair - price) / price * Decimal::from(10_000)
}

/// The same, in dollars for the size traded.
fn edge_usd(side: Side, price: Decimal, size: Decimal, fair: Decimal) -> Decimal {
    sign(side) * (fair - price) * size
}

fn sign(side: Side) -> Decimal {
    match side {
        Side::Buy => Decimal::ONE,
        Side::Sell => -Decimal::ONE,
    }
}

/// "+1.23", "-0.50", or "-" when missing.
fn bps(value: Option<Decimal>) -> String {
    value.map_or_else(
        || "-".to_string(),
        |v| {
            let v = v.round_dp(2);
            if v.is_sign_negative() {
                format!("{v:.2}")
            } else {
                format!("+{v:.2}")
            }
        },
    )
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} {} @ {} | captured {} | 1s {} | 5s {} | 30s {} | 60s {} bps",
            self.side,
            self.size,
            self.price,
            bps(Some(self.captured_bps)),
            bps(self.markout_bps[0]),
            bps(self.markout_bps[1]),
            bps(self.markout_bps[2]),
            bps(self.markout_bps[3]),
        )
    }
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} fills ({} unscored) | captured {} bps (${}) | markout 1s {} | 5s {} | 30s {} | 60s {} bps (${}) | adverse selection at 5s {} bps",
            self.fills,
            self.unscored,
            bps(Some(self.captured_bps)),
            self.captured_usd.round_dp(4),
            bps(self.markout_bps[0]),
            bps(self.markout_bps[1]),
            bps(self.markout_bps[2]),
            bps(self.markout_bps[3]),
            self.markout_60s_usd.round_dp(4),
            bps(self.adverse_5s_bps),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn secs(start: Instant, s: f64) -> Instant {
        start + Duration::from_secs_f64(s)
    }

    /// Feed `fair` every 100 ms from `from` to `to` seconds after `start`.
    fn feed(m: &mut Markouts, start: Instant, from: f64, to: f64, fair: &str) -> Vec<Report> {
        let mut done = Vec::new();
        let mut t = from;
        while t <= to + 1e-9 {
            done.extend(m.on_fair(d(fair), secs(start, t)));
            t += 0.1;
        }
        done
    }

    #[test]
    fn buying_below_fair_captures_the_spread() {
        let mut m = Markouts::default();
        let now = Instant::now();
        m.on_fill(Side::Buy, d("100"), d("1"), Some(d("100.1")), now);
        let s = m.summary().unwrap();
        assert_eq!(s.captured_bps, d("10"));
        assert_eq!(s.captured_usd, d("0.1"));
    }

    #[test]
    fn selling_above_fair_also_captures_the_spread() {
        let mut m = Markouts::default();
        let now = Instant::now();
        m.on_fill(Side::Sell, d("100.1"), d("1"), Some(d("100")), now);
        assert!(m.summary().unwrap().captured_bps > Decimal::ZERO);
    }

    #[test]
    fn each_horizon_waits_for_its_time() {
        let mut m = Markouts::default();
        let start = Instant::now();
        m.on_fill(Side::Buy, d("100"), d("1"), Some(d("100")), start);
        feed(&mut m, start, 0.1, 0.9, "100.2");
        assert_eq!(m.summary().unwrap().markout_bps[0], None, "too early");
        feed(&mut m, start, 1.0, 1.0, "100.2");
        assert_eq!(m.summary().unwrap().markout_bps[0], Some(d("20")));
        assert_eq!(m.summary().unwrap().markout_bps[1], None, "5 s not yet");
    }

    #[test]
    fn the_price_moving_against_us_is_a_negative_markout() {
        // We sold at fair, then the price rose: the buyer knew better.
        let mut m = Markouts::default();
        let start = Instant::now();
        m.on_fill(Side::Sell, d("100"), d("2"), Some(d("100")), start);
        feed(&mut m, start, 0.1, 1.0, "100.2");
        let s = m.summary().unwrap();
        assert_eq!(s.markout_bps[0], Some(d("-20")));
    }

    #[test]
    fn a_fill_is_reported_once_every_horizon_is_measured() {
        let mut m = Markouts::default();
        let start = Instant::now();
        m.on_fill(Side::Buy, d("100"), d("1"), Some(d("100.1")), start);
        assert!(feed(&mut m, start, 0.1, 59.9, "100.3").is_empty());
        let done = feed(&mut m, start, 60.0, 60.0, "100.3");
        assert_eq!(
            done,
            vec![Report {
                side: Side::Buy,
                price: d("100"),
                size: d("1"),
                captured_bps: d("10"),
                markout_bps: [Some(d("30")); 4],
            }]
        );
        let s = m.summary().unwrap();
        // Captured +10, worth +30 five seconds later: the market gave us +20 more.
        assert_eq!(s.adverse_5s_bps, Some(d("20")));
        assert_eq!(s.markout_60s_usd, d("0.3"));
        assert!(
            m.on_fair(d("100.3"), secs(start, 61.0)).is_empty(),
            "reported once"
        );
    }

    #[test]
    fn a_horizon_measured_too_late_is_skipped() {
        // Binance went quiet: the first fair value arrives 3 s after the fill.
        let mut m = Markouts::default();
        let start = Instant::now();
        m.on_fill(Side::Buy, d("100"), d("1"), Some(d("100")), start);
        feed(&mut m, start, 3.0, 60.0, "100.1");
        let s = m.summary().unwrap();
        assert_eq!(s.markout_bps[0], None, "1 s was missed");
        assert_eq!(s.markout_bps[1], Some(d("10")), "5 s was on time");
    }

    #[test]
    fn a_fill_without_a_fair_value_is_counted_but_not_scored() {
        let mut m = Markouts::default();
        m.on_fill(Side::Buy, d("100"), d("1"), None, Instant::now());
        assert_eq!(m.summary(), None, "nothing measured yet");
        m.on_fill(Side::Buy, d("100"), d("1"), Some(d("100")), Instant::now());
        assert_eq!(m.summary().unwrap().unscored, 1);
    }

    #[test]
    fn averages_over_all_fills() {
        let mut m = Markouts::default();
        let start = Instant::now();
        // A buy and a sell at the same price: when the price rises 5 bps,
        // the buy gains what the sell loses.
        m.on_fill(Side::Buy, d("100"), d("1"), Some(d("100")), start);
        m.on_fill(Side::Sell, d("100"), d("1"), Some(d("100")), start);
        feed(&mut m, start, 0.1, 1.0, "100.05");
        let s = m.summary().unwrap();
        assert_eq!(s.fills, 2);
        assert_eq!(
            s.markout_bps[0],
            Some(Decimal::ZERO),
            "+5 and -5 average to 0"
        );
    }
}
