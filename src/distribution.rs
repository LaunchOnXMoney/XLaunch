//! One checked transfer per recipient, grouped by measured size and simulation.
use crate::{
    allocation::Payout,
    chain_instruction, chain_key, pump_key,
    transaction::{Envelope, fits},
};
use anchor_spl::token_2022::spl_token_2022::{
    extension::{BaseStateWithExtensions, ExtensionType, StateWithExtensions},
    instruction::transfer_checked,
    state::Mint,
};
use anyhow::{Context, Result, ensure};
use pump_rust_client::{constants, pda, token};
use solana_sdk::{
    account::Account, instruction::Instruction, pubkey::Pubkey, transaction::VersionedTransaction,
};
use std::collections::BTreeSet;

pub struct Distribution {
    groups: Vec<Vec<Instruction>>,
    payouts: Vec<Payout>,
}

#[derive(Debug)]
pub enum Simulation {
    Success { units_consumed: u64 },
    ResourceLimit { detail: String },
}

/// Business/program failures and RPC errors return Err, never a smaller batch.
pub trait Simulator {
    fn simulate(&self, transaction: &VersionedTransaction) -> Result<Simulation>;
}

#[derive(Debug)]
pub struct Batch {
    pub instructions: Vec<Instruction>,
    pub payouts: Vec<Payout>,
    pub start: usize,
    pub end: usize,
    pub units_consumed: u64,
}

impl Distribution {
    /// Mint account must be fetched after verifying the launch receipt. Refuse transfer
    /// hooks/fees and any other unreviewed extension rather than misdeliver tokens.
    pub fn new(
        mint: Pubkey,
        mint_account: &Account,
        payer: Pubkey,
        authority: Pubkey,
        payouts: Vec<Payout>,
    ) -> Result<Self> {
        ensure!(
            mint_account.owner == chain_key(constants::SPL_TOKEN_2022_PROGRAM_ID)
                && !mint_account.executable,
            "not a Token-2022 mint"
        );
        let state = StateWithExtensions::<Mint>::unpack(&mint_account.data)?;
        ensure!(state.base.decimals == 6, "unexpected base decimals");
        for extension in state.get_extension_types()? {
            ensure!(
                matches!(
                    extension,
                    ExtensionType::MetadataPointer | ExtensionType::TokenMetadata
                ),
                "unreviewed mint extension: {extension:?}"
            );
        }
        let mut seen = BTreeSet::new();
        let mut total = 0u64;
        let mint = pump_key(mint);
        let payer = pump_key(payer);
        let authority = pump_key(authority);
        ensure!(
            payer != Default::default() && authority != Default::default(),
            "invalid signer"
        );
        let program = constants::SPL_TOKEN_2022_PROGRAM_ID;
        let source = pda::associated_token(&authority, &program, &mint).0;
        let mut groups = Vec::with_capacity(payouts.len());
        for payout in &payouts {
            ensure!(
                payout.amount > 0
                    && payout.wallet != Pubkey::default()
                    && seen.insert(payout.wallet),
                "zero or duplicate payout"
            );
            ensure!(
                pump_key(payout.wallet) != authority,
                "recipient is the distribution source owner"
            );
            total = total
                .checked_add(payout.amount)
                .context("payout total overflow")?;
            let wallet = pump_key(payout.wallet);
            let destination = pda::associated_token(&wallet, &program, &mint).0;
            groups.push(vec![
                chain_instruction(token::create_associated_token_account_idempotent(
                    &payer, &wallet, &mint, &program,
                )),
                chain_instruction(transfer_checked(
                    &program,
                    &source,
                    &mint,
                    &destination,
                    &authority,
                    &[],
                    payout.amount,
                    state.base.decimals,
                )?),
            ]);
        }
        ensure!(total <= state.base.supply, "payouts exceed mint supply");
        Ok(Self { groups, payouts })
    }

    pub fn len(&self) -> usize {
        self.groups.len()
    }
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Inspect all instructions; caller must still compile, measure and simulate.
    pub fn all_instructions(&self) -> Vec<Instruction> {
        self.groups.iter().flatten().cloned().collect()
    }

    /// Largest contiguous prefix fitting the envelope AND actual RPC simulation.
    /// Plan the next batch after verifying the previous receipt (shared source ATA).
    pub fn next_batch(
        &self,
        start: usize,
        envelope: &Envelope,
        simulator: &impl Simulator,
    ) -> Result<Batch> {
        ensure!(start < self.groups.len(), "batch cursor out of range");
        let mut body = Vec::new();
        let mut end = start;
        while end < self.groups.len() {
            body.extend_from_slice(&self.groups[end]);
            let tx = envelope.compile(&body)?;
            if !fits(&tx)? {
                body.truncate(body.len() - self.groups[end].len());
                break;
            }
            end += 1;
        }
        ensure!(
            end > start,
            "single recipient does not fit transaction limits"
        );
        loop {
            match simulator.simulate(&envelope.compile(&body)?)? {
                Simulation::Success { units_consumed } => {
                    return Ok(Batch {
                        instructions: body,
                        payouts: self.payouts[start..end].to_vec(),
                        start,
                        end,
                        units_consumed,
                    });
                }
                Simulation::ResourceLimit { detail } => {
                    ensure!(
                        end > start + 1,
                        "single recipient exceeds runtime limits: {detail}"
                    );
                    end -= 1;
                    body.truncate(body.len() - self.groups[end].len());
                }
            }
        }
    }
}
