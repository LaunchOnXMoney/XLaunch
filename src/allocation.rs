//! Integer-only arrival-ordered weights and exact conservation of actual receipts.
use crate::{
    pump_key,
    state::{Config, QuoteAsset},
};
use anyhow::{Context, Result, ensure};
use pump_rust_client::{
    PumpSdk,
    math::{bonding_curve, fees},
    state::BondingCurve,
};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Contribution {
    pub transfer_id: String,
    /// Assigned by the payment adapter from a complete, settled event stream.
    /// Wall-clock processing time is not an ordering source.
    pub sequence: u64,
    pub wallet: Pubkey,
    /// Net funded USDC allocated to the buy, in micro-USDC, excluding operating costs.
    pub quote_budget: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Allocation {
    pub contribution: Contribution,
    pub weight: u64,
    pub curve_quote: u64,
    pub fee_quote: u64,
    pub unspent_quote: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Payout {
    pub wallet: Pubkey,
    pub amount: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BuyQuote {
    pub tokens: u64,
    pub curve_quote: u64,
    pub total_quote: u64,
}

pub fn initial_curve(config: &Config, creator: Pubkey) -> Result<BondingCurve> {
    initial_curve_for(config, creator, QuoteAsset::Usdc)
}

pub fn initial_curve_for(
    config: &Config,
    creator: Pubkey,
    asset: QuoteAsset,
) -> Result<BondingCurve> {
    ensure!(creator != Pubkey::default(), "creator cannot be zero");
    Ok(PumpSdk::initial_bonding_curve(
        &config.global,
        pump_key(creator),
        pump_key(asset.mint()),
        config.initial_quote_reserves(asset),
        false,
        false,
        0,
    ))
}

/// Largest exact-output buy whose fee-inclusive cost fits the budget.
/// The SDK's approximate exact-input quote can exceed a budget after fee ceilings.
pub fn affordable_buy(config: &Config, curve: &BondingCurve, budget: u64) -> Result<BuyQuote> {
    ensure!(
        (curve.quote_mint == fees::USDC_MINT || fees::is_sol_like_quote_mint(&curve.quote_mint))
            && !curve.is_mayhem_mode
            && !curve.is_cashback_coin
            && curve.creator_fee_bps == 0
            && curve.creator != Default::default()
            && curve.token_total_supply == config.global.token_total_supply,
        "only ordinary USDC or SOL curves are supported"
    );
    ensure!(
        curve.virtual_quote_reserves > 0
            && curve.real_token_reserves < curve.virtual_token_reserves,
        "invalid curve"
    );
    let cost = |tokens: u64| -> Result<BuyQuote> {
        if tokens == 0 {
            return Ok(BuyQuote::default());
        }
        let net = u128::from(tokens) * u128::from(curve.virtual_quote_reserves)
            / u128::from(curve.virtual_token_reserves - tokens)
            + 1;
        let bps = fees::compute_bonding_curve_fee_bps(
            &config.global,
            Some(&config.fees),
            &curve.quote_mint,
            0,
            config.global.token_total_supply,
            curve.virtual_quote_reserves,
            curve.virtual_token_reserves,
        );
        let gross = net
            + fees::fee_amount(net, bps.protocol_fee_bps)
            + fees::creator_fee_amount(&curve.creator, net, bps.creator_fee_bps);
        Ok(BuyQuote {
            tokens,
            curve_quote: net.try_into()?,
            total_quote: gross.try_into()?,
        })
    };
    let (mut low, mut high) = (0, curve.real_token_reserves);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if cost(mid)?.total_quote <= budget {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let quote = cost(low)?;
    // Independent public SDK quote must agree with the checked arithmetic.
    let sdk = bonding_curve::buy_sol_amount_from_token_amount(
        &config.global,
        Some(&config.fees),
        curve,
        config.global.token_total_supply,
        low,
    )
    .map_err(|e| anyhow::anyhow!("Pump quote: {e:?}"))?;
    ensure!(
        sdk == quote.total_quote,
        "SDK quote disagrees; stop settlement"
    );
    Ok(quote)
}

/// Replays a sealed ordered input. Never silently sorts, deduplicates, or skips money.
pub fn allocate(
    config: &Config,
    creator: Pubkey,
    inputs: &[Contribution],
) -> Result<Vec<Allocation>> {
    let mut curve = initial_curve(config, creator)?;
    let mut seen = BTreeSet::new();
    let mut previous = None;
    let mut result = Vec::with_capacity(inputs.len());
    for payment in inputs {
        ensure!(
            !payment.transfer_id.is_empty() && seen.insert(payment.transfer_id.clone()),
            "empty or duplicate transfer ID"
        );
        ensure!(
            previous.is_none_or(|p| payment.sequence > p),
            "contributions must be strictly ordered"
        );
        ensure!(
            payment.wallet != Pubkey::default() && payment.quote_budget > 0,
            "invalid wallet or budget"
        );
        previous = Some(payment.sequence);
        let quote = affordable_buy(config, &curve, payment.quote_budget)?;
        curve.virtual_token_reserves -= quote.tokens;
        curve.real_token_reserves -= quote.tokens;
        curve.virtual_quote_reserves = curve
            .virtual_quote_reserves
            .checked_add(quote.curve_quote)
            .context("reserve overflow")?;
        curve.real_quote_reserves = curve
            .real_quote_reserves
            .checked_add(quote.curve_quote)
            .context("reserve overflow")?;
        curve.complete = curve.real_token_reserves == 0;
        result.push(Allocation {
            contribution: payment.clone(),
            weight: quote.tokens,
            curve_quote: quote.curve_quote,
            fee_quote: quote.total_quote - quote.curve_quote,
            unspent_quote: payment.quote_budget - quote.total_quote,
        });
    }
    Ok(result)
}

/// Largest-remainder allocation of the actual verified buy receipt.
/// Group wallets only after pricing every ordered payment. Ties use first arrival.
pub fn distribute(allocations: &[Allocation], actual_tokens: u64) -> Result<Vec<Payout>> {
    let mut wallets = BTreeMap::<Pubkey, (u128, u64)>::new();
    let mut total = 0u128;
    for row in allocations {
        if row.weight == 0 {
            continue;
        }
        total = total
            .checked_add(u128::from(row.weight))
            .context("weight overflow")?;
        let entry = wallets
            .entry(row.contribution.wallet)
            .or_insert((0, row.contribution.sequence));
        entry.0 = entry
            .0
            .checked_add(u128::from(row.weight))
            .context("weight overflow")?;
        entry.1 = entry.1.min(row.contribution.sequence);
    }
    ensure!(
        total > 0 || actual_tokens == 0,
        "tokens received without allocations"
    );
    if total == 0 {
        return Ok(vec![]);
    }
    let mut rows = Vec::new();
    let mut assigned = 0u64;
    for (wallet, (weight, sequence)) in wallets {
        let scaled = weight
            .checked_mul(u128::from(actual_tokens))
            .context("allocation overflow")?;
        let amount = u64::try_from(scaled / total)?;
        assigned = assigned.checked_add(amount).context("payout overflow")?;
        rows.push((Payout { wallet, amount }, scaled % total, sequence));
    }
    rows.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then(a.2.cmp(&b.2))
            .then(a.0.wallet.cmp(&b.0.wallet))
    });
    let remaining = usize::try_from(actual_tokens - assigned)?;
    ensure!(remaining <= rows.len(), "invalid allocation remainder");
    for row in rows.iter_mut().take(remaining) {
        row.0.amount += 1;
    }
    rows.sort_by_key(|r| (r.2, r.0.wallet));
    Ok(rows
        .into_iter()
        .map(|r| r.0)
        .filter(|p| p.amount > 0)
        .collect())
}
