//! Strict decoding of the pinned Pump interface; source record: docs/rust-settlement.md.
use anchor_lang::{
    AccountSerialize, AnchorDeserialize, Discriminator, solana_program::program_pack::Pack,
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use pump_rust_client::{
    constants,
    math::fees::USDC_MINT,
    pda, pump,
    state::{FeeConfig, Global},
};
use serde_json::Value;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{account::Account, pubkey::Pubkey};

use crate::chain_key;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum QuoteAsset {
    Sol,
    Usdc,
}

impl QuoteAsset {
    pub fn mint(self) -> solana_sdk::pubkey::Pubkey {
        chain_key(match self {
            Self::Sol => constants::NATIVE_MINT,
            Self::Usdc => USDC_MINT,
        })
    }
}

pub struct Config {
    pub(crate) global: Global,
    pub(crate) fees: FeeConfig,
    pub slot: u64,
    snapshot: ConfigSnapshot,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ConfigSnapshot {
    slot: u64,
    global: Vec<u8>,
    fees: Vec<u8>,
}

impl Config {
    pub fn usdc_mint() -> Pubkey {
        chain_key(USDC_MINT)
    }

    pub fn from_accounts(
        slot: u64,
        global: &Account,
        fees: &Account,
        usdc: &Account,
    ) -> Result<Self> {
        ensure!(
            !global.executable && !fees.executable,
            "configuration account is executable"
        );
        ensure!(
            usdc.owner == chain_key(constants::SPL_TOKEN_PROGRAM_ID) && !usdc.executable,
            "USDC owner/type mismatch"
        );
        let mint = anchor_spl::token::spl_token::state::Mint::unpack(&usdc.data)?;
        ensure!(mint.decimals == 6, "USDC decimals changed");
        Self::decode(slot, global.owner, &global.data, fees.owner, &fees.data)
    }

    fn decode(
        slot: u64,
        global_owner: Pubkey,
        global_bytes: &[u8],
        fee_owner: Pubkey,
        fee_bytes: &[u8],
    ) -> Result<Self> {
        ensure!(global_owner == chain_key(pump::ID), "Global owner mismatch");
        ensure!(
            fee_owner == chain_key(constants::FEE_PROGRAM_ID),
            "FeeConfig owner mismatch"
        );
        // Deliberately bypass SDK AccountWrapper::try_deserialize: it zero-pads truncation.
        ensure!(
            global_bytes.starts_with(Global::DISCRIMINATOR),
            "Global discriminator mismatch"
        );
        let global = Global::deserialize(&mut &global_bytes[Global::DISCRIMINATOR.len()..])?;
        let mut canonical = Vec::new();
        global.try_serialize(&mut canonical)?;
        ensure!(global_bytes.starts_with(&canonical), "noncanonical Global");
        // Current published IDL adds Pubkey holder_reward_claim_authority + bool.
        // Its fields preceding that extension are identical to the SDK IDL.
        let tail = &global_bytes[canonical.len()..];
        ensure!(
            tail.len() == 33 && tail[32] <= 1,
            "Global layout differs from pinned IDL; reverify"
        );
        ensure!(
            fee_bytes.starts_with(FeeConfig::DISCRIMINATOR),
            "FeeConfig discriminator mismatch"
        );
        let fees = FeeConfig::deserialize(&mut &fee_bytes[FeeConfig::DISCRIMINATOR.len()..])?;
        canonical.clear();
        fees.try_serialize(&mut canonical)?;
        ensure!(
            fee_bytes.starts_with(&canonical)
                && fee_bytes[canonical.len()..].iter().all(|x| *x == 0),
            "unexpected FeeConfig data or padding"
        );
        ensure!(
            global.initialized && global.create_v2_enabled,
            "Pump creation disabled"
        );
        ensure!(
            global.whitelisted_quote_mints.contains(&USDC_MINT),
            "USDC not whitelisted"
        );
        ensure!(
            global.initial_virtual_quote_reserves > 0
                && global.initial_real_token_reserves > 0
                && global.initial_real_token_reserves < global.initial_virtual_token_reserves
                && global.initial_real_token_reserves <= global.token_total_supply,
            "invalid initial reserves"
        );
        // SDK tier quotes for ordinary coins use this fixed supply. Refuse a mismatch.
        ensure!(
            u128::from(global.token_total_supply)
                == pump_rust_client::math::bonding_curve::TOKEN_SUPPLY,
            "supply changed; reverify fee math"
        );
        ensure!(
            !fees.stable_fee_tiers.is_empty() && !fees.fee_tiers.is_empty(),
            "fee schedule missing"
        );
        ensure!(
            global.initial_virtual_sol_reserves > 0,
            "invalid SOL reserves"
        );
        ensure!(
            fees.stable_fee_tiers
                .windows(2)
                .all(|w| w[0].market_cap_lamports_threshold < w[1].market_cap_lamports_threshold),
            "unsorted fee schedule"
        );
        ensure!(
            fees.fee_tiers
                .windows(2)
                .all(|w| w[0].market_cap_lamports_threshold < w[1].market_cap_lamports_threshold),
            "unsorted SOL fee schedule"
        );
        for tier in fees.stable_fee_tiers.iter().chain(&fees.fee_tiers) {
            ensure!(
                tier.fees.protocol_fee_bps <= 10_000 && tier.fees.creator_fee_bps <= 10_000,
                "fee rate outside supported bounds"
            );
        }
        Ok(Self {
            global,
            fees,
            slot,
            snapshot: ConfigSnapshot {
                slot,
                global: global_bytes.to_vec(),
                fees: fee_bytes.to_vec(),
            },
        })
    }

    /// Public accounts at processed commitment in one RPC bank context. Read-only.
    pub fn fetch(helius: &helius::Helius) -> Result<Self> {
        let keys = [
            chain_key(pda::pump::global().0),
            chain_key(pda::pump::fee_config().0),
            Self::usdc_mint(),
        ];
        let response = helius
            .rpc_client
            .solana_client
            .get_multiple_accounts_with_commitment(&keys, CommitmentConfig::processed())?;
        ensure!(response.value.len() == 3, "incomplete config response");
        let accounts: Vec<_> = response
            .value
            .iter()
            .map(|a| a.as_ref().context("missing configuration account"))
            .collect::<Result<_>>()?;
        Self::from_accounts(response.context.slot, accounts[0], accounts[1], accounts[2])
    }

    /// For offline review/tests only: historical public evidence, never a live quote.
    pub fn saved_evidence() -> Result<Self> {
        let global: Value =
            serde_json::from_str(include_str!("../docs/verification/pump-global.rpc.json"))?;
        let fees: Value =
            serde_json::from_str(include_str!("../docs/verification/pump-fees.rpc.json"))?;
        let quote: Value =
            serde_json::from_str(include_str!("../docs/verification/usdc-mint.rpc.json"))?;
        let q = &quote["result"]["value"];
        ensure!(
            q["owner"].as_str() == Some(&constants::SPL_TOKEN_PROGRAM_ID.to_string())
                && q["data"]["parsed"]["info"]["decimals"] == 6
                && q["data"]["parsed"]["info"]["isInitialized"] == true,
            "bad USDC evidence"
        );
        fn unpack(v: &Value) -> Result<(Pubkey, Vec<u8>)> {
            ensure!(v.get("error").is_none(), "RPC error in evidence");
            let a = &v["result"]["value"];
            ensure!(
                a["data"][1] == "base64" && a["executable"] == false,
                "bad account encoding/type"
            );
            Ok((
                a["owner"].as_str().context("owner missing")?.parse()?,
                STANDARD.decode(a["data"][0].as_str().context("data missing")?)?,
            ))
        }
        let (go, gb) = unpack(&global)?;
        let (fo, fb) = unpack(&fees)?;
        Self::decode(
            global["result"]["context"]["slot"]
                .as_u64()
                .context("slot missing")?,
            go,
            &gb,
            fo,
            &fb,
        )
    }

    pub fn initial_reserves(&self) -> (u64, u64, u64) {
        (
            self.global.initial_virtual_token_reserves,
            self.global.initial_virtual_quote_reserves,
            self.global.initial_real_token_reserves,
        )
    }

    pub fn initial_quote_reserves(&self, asset: QuoteAsset) -> u64 {
        match asset {
            QuoteAsset::Sol => self.global.initial_virtual_sol_reserves,
            QuoteAsset::Usdc => self.global.initial_virtual_quote_reserves,
        }
    }

    pub fn snapshot(&self) -> ConfigSnapshot {
        self.snapshot.clone()
    }

    pub fn from_snapshot(snapshot: &ConfigSnapshot) -> Result<Self> {
        Self::decode(
            snapshot.slot,
            chain_key(pump::ID),
            &snapshot.global,
            chain_key(constants::FEE_PROGRAM_ID),
            &snapshot.fees,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bytes(name: &str) -> Vec<u8> {
        let text = match name {
            "global" => include_str!("../docs/verification/pump-global.rpc.json"),
            _ => include_str!("../docs/verification/pump-fees.rpc.json"),
        };
        let value: Value = serde_json::from_str(text).unwrap();
        STANDARD
            .decode(value["result"]["value"]["data"][0].as_str().unwrap())
            .unwrap()
    }
    #[test]
    fn truncated_or_foreign_accounts_are_rejected_without_zero_padding() {
        let global = bytes("global");
        let fees = bytes("fees");
        for len in [0, 8, 900, 1054, global.len() - 1] {
            assert!(
                Config::decode(
                    1,
                    chain_key(pump::ID),
                    &global[..len],
                    chain_key(constants::FEE_PROGRAM_ID),
                    &fees
                )
                .is_err()
            );
        }
        assert!(
            Config::decode(
                1,
                Pubkey::default(),
                &global,
                chain_key(constants::FEE_PROGRAM_ID),
                &fees
            )
            .is_err()
        );
        assert!(
            Config::decode(
                1,
                chain_key(pump::ID),
                &global,
                chain_key(constants::FEE_PROGRAM_ID),
                &fees[..8]
            )
            .is_err()
        );
    }
}
