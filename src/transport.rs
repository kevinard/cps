use std::{
    ffi::OsStr,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use reqwest::{Client, ClientBuilder, redirect::Policy};

const DEADLINE_VARIABLE: &str = "CPS_TRANSFER_DEADLINE_SECONDS";
const MIN_RATE_VARIABLE: &str = "CPS_TRANSFER_MIN_BYTES_PER_SECOND";
const DEFAULT_DEADLINE: Duration = Duration::from_secs(4 * 60 * 60);
const DEFAULT_MIN_RATE: u64 = 4096;
/// Each response body must deliver the minimum rate over every window of this length.
pub(crate) const RATE_WINDOW: Duration = Duration::from_secs(60);
/// The longest a single read may stall before the transfer fails.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn https_client() -> ClientBuilder {
    Client::builder()
        .https_only(true)
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(READ_TIMEOUT)
}

/// Bounds for one mirror operation that holds a repository lock.
///
/// The deadline covers the whole operation, including every metadata and payload
/// request. The throughput floor applies separately to each response body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TransferLimits {
    pub deadline: Duration,
    pub min_rate: u64,
}

impl Default for TransferLimits {
    fn default() -> Self {
        Self {
            deadline: DEFAULT_DEADLINE,
            min_rate: DEFAULT_MIN_RATE,
        }
    }
}

impl TransferLimits {
    /// Read `CPS_TRANSFER_DEADLINE_SECONDS` and `CPS_TRANSFER_MIN_BYTES_PER_SECOND`.
    pub(crate) fn from_env() -> Result<Self> {
        Self::parse(
            std::env::var_os(DEADLINE_VARIABLE).as_deref(),
            std::env::var_os(MIN_RATE_VARIABLE).as_deref(),
        )
    }

    fn parse(deadline: Option<&OsStr>, min_rate: Option<&OsStr>) -> Result<Self> {
        fn number(name: &str, value: &OsStr) -> Result<u64> {
            value
                .to_str()
                .filter(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|value| value.parse().ok())
                .with_context(|| format!("{name} must be a decimal integer"))
        }
        let mut limits = Self::default();
        if let Some(value) = deadline {
            let seconds = number(DEADLINE_VARIABLE, value)?;
            ensure!(
                (1..=7 * 24 * 60 * 60).contains(&seconds),
                "{DEADLINE_VARIABLE} must be between 1 second and 7 days"
            );
            limits.deadline = Duration::from_secs(seconds);
        }
        if let Some(value) = min_rate {
            limits.min_rate = number(MIN_RATE_VARIABLE, value)?;
        }
        Ok(limits)
    }

    pub(crate) fn start(self) -> TransferBudget {
        TransferBudget {
            deadline: Instant::now() + self.deadline,
            limits: self,
        }
    }
}

/// A started operation deadline shared by every request in that operation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TransferBudget {
    deadline: Instant,
    limits: TransferLimits,
}

impl TransferBudget {
    /// Time left before the operation deadline, or an error once it has passed.
    pub(crate) fn remaining(&self) -> std::io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| self.deadline_error())
    }

    fn deadline_error(&self) -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "mirror transfer exceeded the {} second deadline ({DEADLINE_VARIABLE})",
                self.limits.deadline.as_secs()
            ),
        )
    }

    pub(crate) fn meter(&self) -> RateMeter {
        RateMeter::new(*self, Instant::now())
    }
}

/// Enforces the operation deadline and the throughput floor for one response body.
#[derive(Debug)]
pub(crate) struct RateMeter {
    budget: TransferBudget,
    window_start: Instant,
    window_bytes: u64,
}

impl RateMeter {
    fn new(budget: TransferBudget, now: Instant) -> Self {
        Self {
            budget,
            window_start: now,
            window_bytes: 0,
        }
    }

    /// Account for a received chunk. Stalls are bounded by the read timeout, so a
    /// check on each arrival catches a body that trickles below the floor.
    pub(crate) fn record(&mut self, bytes: usize) -> std::io::Result<()> {
        self.record_at(bytes, Instant::now())
    }

    fn record_at(&mut self, bytes: usize, now: Instant) -> std::io::Result<()> {
        if now >= self.budget.deadline {
            return Err(self.budget.deadline_error());
        }
        self.window_bytes = self.window_bytes.saturating_add(bytes as u64);
        let elapsed = now.saturating_duration_since(self.window_start);
        if elapsed >= RATE_WINDOW {
            let required = self
                .budget
                .limits
                .min_rate
                .saturating_mul(RATE_WINDOW.as_secs());
            // Count the chunk that ends the window, but not a long stall before it.
            if self.window_bytes < required {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "mirror transfer fell below {} bytes per second ({MIN_RATE_VARIABLE})",
                        self.budget.limits.min_rate
                    ),
                ));
            }
            self.window_start = now;
            self.window_bytes = 0;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_parse_overrides_and_reject_invalid_values() -> Result<()> {
        assert_eq!(
            TransferLimits::parse(None, None)?,
            TransferLimits::default()
        );
        let limits = TransferLimits::parse(Some(OsStr::new("90")), Some(OsStr::new("0")))?;
        assert_eq!(limits.deadline, Duration::from_secs(90));
        assert_eq!(limits.min_rate, 0);
        for invalid in ["", "0", "-1", "1e3", "999999999"] {
            assert!(TransferLimits::parse(Some(OsStr::new(invalid)), None).is_err());
        }
        assert!(TransferLimits::parse(None, Some(OsStr::new("fast"))).is_err());
        Ok(())
    }

    #[test]
    fn meter_rejects_a_trickling_body_and_the_deadline() {
        let limits = TransferLimits {
            deadline: Duration::from_secs(600),
            min_rate: 100,
        };
        let start = Instant::now();
        let budget = TransferBudget {
            deadline: start + limits.deadline,
            limits,
        };
        let mut meter = RateMeter::new(budget, start);
        // A fast first window cannot buy time for a later trickle.
        meter
            .record_at(1_000_000, start + Duration::from_secs(1))
            .unwrap();
        meter
            .record_at(1, start + Duration::from_secs(61))
            .expect("the first window met the floor");
        meter
            .record_at(10, start + Duration::from_secs(90))
            .expect("the window is still open");
        let error = meter
            .record_at(10, start + Duration::from_secs(125))
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);

        let mut meter = RateMeter::new(budget, start);
        meter
            .record_at(6000, start + Duration::from_secs(60))
            .expect("exactly the floor");
        let error = meter
            .record_at(1_000_000, start + Duration::from_secs(600))
            .unwrap_err();
        assert!(error.to_string().contains("deadline"));
        assert!(budget.remaining().is_ok());
    }
}
