//! Validate transaction-local receipts against the exact signed bytes.
use crate::{
    allocation::Payout,
    chain_key,
    launch::{Launch, LaunchPlan},
    pump_key,
    state::{Config, QuoteAsset},
    transaction::SignedTransaction,
};
use anchor_lang::{AnchorDeserialize, Discriminator};
use anyhow::{Context, Result, ensure};
use pump_rust_client::{constants, pda};
use serde::Serialize;
use serde_json::Value;
use solana_sdk::{message::VersionedMessage, pubkey::Pubkey};
use solana_transaction_status_client_types::EncodedConfirmedTransactionWithStatusMeta;

#[derive(Debug, Serialize)]
pub struct LaunchReceipt {
    pub signature: String,
    pub slot: u64,
    pub quote_asset: QuoteAsset,
    pub tokens_received: u64,
    pub quote_spent: u64,
    pub unspent_quote: u64,
}

/// Validate supplied transaction metadata against the exact signed transaction.
/// The caller owns commitment tracking; this function does not promote a
/// processed observation to confirmed/finalized.
/// Never substitute a projected quote or a later wallet balance for metadata.
/// Standard mainnet getTransaction cannot supply processed metadata: use a
/// verified transaction stream adapter. No stronger-commitment fallback is made.
pub fn verify_launch_receipt(
    response: &EncodedConfirmedTransactionWithStatusMeta,
    signed: &SignedTransaction,
    launch: &Launch,
    plan: &LaunchPlan,
) -> Result<LaunchReceipt> {
    let (slot, keys, meta) = verified_metadata(response, signed)?;
    launch_receipt(
        signed.signature().to_string(),
        slot,
        &keys,
        &meta,
        launch,
        plan,
    )
}

fn verified_metadata(
    response: &EncodedConfirmedTransactionWithStatusMeta,
    signed: &SignedTransaction,
) -> Result<(u64, Vec<String>, Value)> {
    let decoded = response
        .transaction
        .transaction
        .decode()
        .context("RPC transaction decode failed")?;
    ensure!(
        bincode::serialize(&decoded)? == signed.wire()?,
        "RPC transaction differs from journaled bytes"
    );
    let meta = response
        .transaction
        .meta
        .as_ref()
        .context("transaction metadata missing")?;
    ensure!(
        meta.err.is_none(),
        "settlement transaction failed: {:?}",
        meta.err
    );
    let meta = serde_json::to_value(meta)?;
    let VersionedMessage::V0(message) = &decoded.message else {
        anyhow::bail!("only V0 supported");
    };
    let mut keys: Vec<String> = message
        .account_keys
        .iter()
        .map(ToString::to_string)
        .collect();
    for field in ["writable", "readonly"] {
        let loaded = meta["loadedAddresses"][field]
            .as_array()
            .context("loaded addresses missing")?;
        for key in loaded {
            keys.push(key.as_str().context("invalid loaded address")?.to_owned());
        }
    }
    Ok((response.slot, keys, meta))
}

fn launch_receipt(
    signature: String,
    slot: u64,
    keys: &[String],
    meta: &Value,
    launch: &Launch,
    plan: &LaunchPlan,
) -> Result<LaunchReceipt> {
    let token_account = chain_key(
        pda::associated_token(
            &pump_key(launch.buyer),
            &constants::SPL_TOKEN_2022_PROGRAM_ID,
            &pump_key(launch.mint),
        )
        .0,
    );
    ensure!(
        plan.token_account == token_account,
        "launch plan/recipient mismatch"
    );
    let quote_account = chain_key(
        pda::associated_token(
            &pump_key(launch.buyer),
            &constants::SPL_TOKEN_PROGRAM_ID,
            &pump_key(Config::usdc_mint()),
        )
        .0,
    );
    let token_index = keys
        .iter()
        .position(|k| *k == token_account.to_string())
        .context("base ATA absent from transaction")?;
    let pre = meta["preTokenBalances"]
        .as_array()
        .context("pre token balances missing")?;
    let post = meta["postTokenBalances"]
        .as_array()
        .context("post token balances missing")?;
    ensure!(
        balance(
            pre,
            token_index,
            launch.mint,
            launch.buyer,
            chain_key(constants::SPL_TOKEN_2022_PROGRAM_ID)
        )?
        .is_none(),
        "reserved mint already had a token balance before launch"
    );
    let received = balance(
        post,
        token_index,
        launch.mint,
        launch.buyer,
        chain_key(constants::SPL_TOKEN_2022_PROGRAM_ID),
    )?
    .context("base receipt missing")?;
    ensure!(
        received == plan.quote.tokens,
        "actual base receipt differs from exact-output buy"
    );
    if launch.quote_asset == QuoteAsset::Sol {
        let event = sol_trade_event(keys, meta, launch)?;
        ensure!(
            event.token_amount == received && event.sol_amount == plan.quote.curve_quote,
            "SOL execution differs from quoted output/input"
        );
        let spent = event
            .sol_amount
            .checked_add(event.fee)
            .and_then(|x| x.checked_add(event.creator_fee))
            .context("SOL receipt overflow")?;
        ensure!(
            spent == plan.quote.total_quote && spent <= launch.quote_budget,
            "SOL buy cost mismatch"
        );
        return Ok(LaunchReceipt {
            signature,
            slot,
            quote_asset: QuoteAsset::Sol,
            tokens_received: received,
            quote_spent: spent,
            unspent_quote: launch.quote_budget - spent,
        });
    }
    let quote_index = keys
        .iter()
        .position(|k| *k == quote_account.to_string())
        .context("USDC ATA absent from transaction")?;
    let before = balance(
        pre,
        quote_index,
        Config::usdc_mint(),
        launch.buyer,
        chain_key(constants::SPL_TOKEN_PROGRAM_ID),
    )?
    .context("pre USDC balance missing")?;
    let after = balance(
        post,
        quote_index,
        Config::usdc_mint(),
        launch.buyer,
        chain_key(constants::SPL_TOKEN_PROGRAM_ID),
    )?
    .context("post USDC balance missing")?;
    let spent = before
        .checked_sub(after)
        .context("USDC balance increased unexpectedly")?;
    ensure!(
        spent > 0 && spent <= launch.quote_budget,
        "USDC debit outside authorized budget"
    );
    Ok(LaunchReceipt {
        signature,
        slot,
        quote_asset: QuoteAsset::Usdc,
        tokens_received: received,
        quote_spent: spent,
        unspent_quote: launch.quote_budget - spent,
    })
}

/// Read authenticated Pump self-CPI event data, not free-form log strings.
/// Pinned current IDL adds two u64 holder-reward fields to the SDK event.
fn sol_trade_event(
    keys: &[String],
    meta: &Value,
    launch: &Launch,
) -> Result<pump_rust_client::pump::events::TradeEvent> {
    use pump_rust_client::pump::{self, events::TradeEvent};
    let mut found = None;
    for group in meta["innerInstructions"]
        .as_array()
        .context("inner instructions missing")?
    {
        for ix in group["instructions"]
            .as_array()
            .context("inner group malformed")?
        {
            let index = usize::try_from(
                ix["programIdIndex"]
                    .as_u64()
                    .context("compiled program index missing")?,
            )?;
            if keys.get(index) != Some(&pump::ID.to_string()) {
                continue;
            }
            let bytes =
                bs58::decode(ix["data"].as_str().context("inner data missing")?).into_vec()?;
            let Some(bytes) = bytes.strip_prefix(anchor_lang::event::EVENT_IX_TAG_LE) else {
                continue;
            };
            let Some(mut bytes) = bytes.strip_prefix(TradeEvent::DISCRIMINATOR) else {
                continue;
            };
            let event = TradeEvent::deserialize(&mut bytes)?;
            ensure!(
                bytes.len() == 16 && bytes.iter().all(|b| *b == 0),
                "unreviewed holder-reward event layout/rate"
            );
            ensure!(
                event.is_buy
                    && event.mint == pump_key(launch.mint)
                    && event.user == pump_key(launch.buyer)
                    && event.creator == pump_key(launch.creator)
                    && !event.mayhem_mode
                    && event.cashback == 0
                    && event.shareholders.is_empty(),
                "unexpected trade event identity/mode"
            );
            ensure!(
                pump_rust_client::math::fees::is_sol_like_quote_mint(&event.quote_mint),
                "non-SOL event"
            );
            ensure!(found.is_none(), "multiple trade events");
            found = Some(event);
        }
    }
    found.context("Pump SOL trade event missing")
}

#[derive(Debug, Serialize)]
pub struct PayoutReceipt {
    pub signature: String,
    pub slot: u64,
    pub delivered_tokens: u64,
    pub recipients: usize,
}

/// Verify a supplied receipt without making an RPC request or assuming finality.
/// Every recipient delta and the matching source debit must reconcile before
/// the caller advances its persisted batch cursor.
pub fn verify_payout_receipt(
    response: &EncodedConfirmedTransactionWithStatusMeta,
    signed: &SignedTransaction,
    mint: Pubkey,
    authority: Pubkey,
    payouts: &[Payout],
) -> Result<PayoutReceipt> {
    let (slot, keys, meta) = verified_metadata(response, signed)?;
    let total = payout_deltas(&keys, &meta, mint, authority, payouts)?;
    Ok(PayoutReceipt {
        signature: signed.signature().to_string(),
        slot,
        delivered_tokens: total,
        recipients: payouts.len(),
    })
}

fn payout_deltas(
    keys: &[String],
    meta: &Value,
    mint: Pubkey,
    authority: Pubkey,
    payouts: &[Payout],
) -> Result<u64> {
    let pre = meta["preTokenBalances"]
        .as_array()
        .context("pre token balances missing")?;
    let post = meta["postTokenBalances"]
        .as_array()
        .context("post token balances missing")?;
    let program = chain_key(constants::SPL_TOKEN_2022_PROGRAM_ID);
    let index = |owner| -> Result<usize> {
        let ata = chain_key(
            pda::associated_token(&pump_key(owner), &pump_key(program), &pump_key(mint)).0,
        );
        keys.iter()
            .position(|key| *key == ata.to_string())
            .context("payout ATA missing from transaction")
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut total = 0u64;
    for payout in payouts {
        ensure!(
            payout.amount > 0 && payout.wallet != authority && seen.insert(payout.wallet),
            "invalid payout manifest"
        );
        let i = index(payout.wallet)?;
        let before = balance(pre, i, mint, payout.wallet, program)?.unwrap_or(0);
        let after =
            balance(post, i, mint, payout.wallet, program)?.context("recipient balance missing")?;
        ensure!(
            after.checked_sub(before) == Some(payout.amount),
            "recipient received a different amount"
        );
        total = total
            .checked_add(payout.amount)
            .context("payout total overflow")?;
    }
    let source = index(authority)?;
    let before =
        balance(pre, source, mint, authority, program)?.context("source pre balance missing")?;
    let after =
        balance(post, source, mint, authority, program)?.context("source post balance missing")?;
    ensure!(
        before.checked_sub(after) == Some(total),
        "source debit differs from payout total"
    );
    Ok(total)
}

fn balance(
    rows: &[Value],
    index: usize,
    mint: Pubkey,
    owner: Pubkey,
    program: Pubkey,
) -> Result<Option<u64>> {
    let rows: Vec<_> = rows
        .iter()
        .filter(|v| v["accountIndex"].as_u64() == Some(index as u64))
        .collect();
    ensure!(rows.len() <= 1, "duplicate token balance record");
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    ensure!(
        row["mint"] == mint.to_string()
            && row["owner"] == owner.to_string()
            && row["programId"] == program.to_string()
            && row["uiTokenAmount"]["decimals"] == 6,
        "token balance identity mismatch"
    );
    Ok(Some(
        row["uiTokenAmount"]["amount"]
            .as_str()
            .context("raw amount missing")?
            .parse()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn payout_reconciliation_requires_exact_recipient_credit_and_source_debit() {
        let mint = Pubkey::new_from_array([1; 32]);
        let source = Pubkey::new_from_array([2; 32]);
        let recipient = Pubkey::new_from_array([3; 32]);
        let program = chain_key(constants::SPL_TOKEN_2022_PROGRAM_ID);
        let keys: Vec<_> = [source, recipient]
            .into_iter()
            .map(|p| {
                chain_key(
                    pda::associated_token(&pump_key(p), &pump_key(program), &pump_key(mint)).0,
                )
                .to_string()
            })
            .collect();
        let row = |index, owner: Pubkey, amount: &str| json!({"accountIndex":index,"mint":mint.to_string(),"owner":owner.to_string(),"programId":program.to_string(),"uiTokenAmount":{"decimals":6,"amount":amount}});
        let mut meta = json!({"preTokenBalances":[row(0,source,"100")],"postTokenBalances":[row(0,source,"90"),row(1,recipient,"10")]});
        let payouts = [Payout {
            wallet: recipient,
            amount: 10,
        }];
        assert_eq!(
            payout_deltas(&keys, &meta, mint, source, &payouts).unwrap(),
            10
        );
        meta["postTokenBalances"][1]["uiTokenAmount"]["amount"] = json!("9");
        assert!(payout_deltas(&keys, &meta, mint, source, &payouts).is_err());
        meta["postTokenBalances"][1]["uiTokenAmount"]["amount"] = json!("10");
        meta["postTokenBalances"][0]["uiTokenAmount"]["amount"] = json!("89");
        assert!(payout_deltas(&keys, &meta, mint, source, &payouts).is_err());
    }
}
