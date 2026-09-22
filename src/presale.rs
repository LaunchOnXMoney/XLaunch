//! USDC contribution ledger, fixed raise cap, and one-hour closing rule.
//! USDC weights are frozen using the opening configuration. SOL funding is a
//! separate settlement input, never inferred from a dollar amount or spot price.
use crate::{
    allocation::{self, Allocation, Contribution},
    state::{Config, ConfigSnapshot},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

pub const RAISE_CAP_MICRO_USDC: u64 = 12_000_000_000;
pub const RAISE_DURATION_SECONDS: u64 = 60 * 60;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trigger {
    CapReached,
    DeadlineElapsed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Payment {
    pub contribution: Contribution,
    /// Settled arrival time supplied by the payment adapter, not processing time.
    pub received_at: u64,
    pub accepted_micro_usdc: u64,
    /// Above-cap and at/after-deadline funds remain owed; never silently spent.
    pub unaccepted_micro_usdc: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Raise {
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub name: String,
    pub symbol: String,
    pub metadata_uri: String,
    pub opened_at: u64,
    pub deadline: u64,
    opening_config: ConfigSnapshot,
    payments: Vec<Payment>,
}

impl Raise {
    pub fn new(
        config: &Config,
        mint: Pubkey,
        creator: Pubkey,
        opened_at: u64,
        name: String,
        symbol: String,
        metadata_uri: String,
    ) -> Result<Self> {
        ensure!(
            mint != Pubkey::default() && creator != Pubkey::default(),
            "invalid mint/creator"
        );
        Ok(Self {
            mint,
            creator,
            name,
            symbol,
            metadata_uri,
            opened_at,
            deadline: opened_at
                .checked_add(RAISE_DURATION_SECONDS)
                .context("deadline overflow")?,
            opening_config: config.snapshot(),
            payments: vec![],
        })
    }
    pub fn accepted_micro_usdc(&self) -> u64 {
        // Each insertion is capped against remaining room, so the sum <= cap.
        self.payments.iter().map(|p| p.accepted_micro_usdc).sum()
    }
    pub fn payments(&self) -> &[Payment] {
        &self.payments
    }
    pub fn record(&mut self, contribution: Contribution, received_at: u64) -> Result<&Payment> {
        ensure!(received_at >= self.opened_at, "arrival precedes opening");
        ensure!(
            contribution.quote_budget > 0
                && contribution.wallet != Pubkey::default()
                && !contribution.transfer_id.is_empty(),
            "invalid contribution"
        );
        ensure!(
            self.payments
                .iter()
                .all(|p| p.contribution.transfer_id != contribution.transfer_id),
            "duplicate payment"
        );
        if let Some(previous) = self.payments.last() {
            ensure!(
                contribution.sequence > previous.contribution.sequence
                    && received_at >= previous.received_at,
                "payment arrival order is ambiguous"
            );
        }
        let accepted = if received_at < self.deadline {
            contribution
                .quote_budget
                .min(RAISE_CAP_MICRO_USDC - self.accepted_micro_usdc())
        } else {
            0
        };
        self.payments.push(Payment {
            unaccepted_micro_usdc: contribution.quote_budget - accepted,
            contribution,
            received_at,
            accepted_micro_usdc: accepted,
        });
        Ok(self.payments.last().unwrap())
    }
    pub fn trigger(&self, now: u64) -> Option<Trigger> {
        if self.accepted_micro_usdc() == RAISE_CAP_MICRO_USDC {
            Some(Trigger::CapReached)
        } else if now >= self.deadline {
            Some(Trigger::DeadlineElapsed)
        } else {
            None
        }
    }
    pub fn allocations(&self) -> Result<Vec<Allocation>> {
        let config = Config::from_snapshot(&self.opening_config)?;
        let contributions: Vec<_> = self
            .payments
            .iter()
            .filter(|p| p.accepted_micro_usdc > 0)
            .map(|p| {
                let mut c = p.contribution.clone();
                c.quote_budget = p.accepted_micro_usdc;
                c
            })
            .collect();
        allocation::allocate(&config, self.creator, &contributions)
    }
}

/// Completed token distribution evidence, returned by the isolated executor.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settlement {
    pub launch_signature: String,
    pub payout_signatures: Vec<String>,
    pub tokens_received: u64,
    pub tokens_distributed: u64,
}

/// The executor supplies funded lamports, reserved mint signer, fresh config,
/// Helius submission, and verified receipts. CapReached requires a full buyout;
/// DeadlineElapsed buys what the partial raise's funded SOL budget affords.
pub trait Executor {
    fn settle(&mut self, raise: &Raise, trigger: Trigger) -> Result<Settlement>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Status {
    Open,
    Settling { trigger: Trigger },
    Distributed { receipt: Settlement },
    Empty,
    NeedsReconciliation { error: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub raise: Raise,
    pub status: Status,
}
