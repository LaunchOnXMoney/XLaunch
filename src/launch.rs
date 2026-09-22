//! Pure Pump instruction construction. No key generation, RPC, signing, or sending.
use crate::{
    allocation::{BuyQuote, affordable_buy, initial_curve_for},
    chain_instruction, chain_key, pump_key,
    state::{Config, QuoteAsset},
};
use anyhow::{Context, Result, ensure};
use pump_rust_client::{PumpSdk, constants, math::fees::USDC_MINT, pda, token};
use solana_sdk::{instruction::Instruction, pubkey::Pubkey};

#[derive(Clone, Debug)]
pub struct Launch {
    /// Reserved mint keypair public key; caller retains the signer securely.
    pub mint: Pubkey,
    pub buyer: Pubkey,
    pub creator: Pubkey,
    pub name: String,
    pub symbol: String,
    pub metadata_uri: String,
    pub quote_asset: QuoteAsset,
    /// Lamports for SOL; micro-USDC for USDC. Never pass a raise total as lamports.
    pub quote_budget: u64,
}

pub struct LaunchPlan {
    pub instructions: Vec<Instruction>,
    pub quote: BuyQuote,
    pub unspent_quote: u64,
    pub token_account: Pubkey,
    pub config_slot: u64,
}

/// Build the active 12,000-USDC product's SOL settlement. The supplied lamports
/// must already be funded and attributed to this raise. No exchange-rate guess.
pub fn for_raise(
    config: &Config,
    raise: &crate::presale::Raise,
    trigger: crate::presale::Trigger,
    buyer: Pubkey,
    funded_lamports: u64,
) -> Result<(Launch, LaunchPlan)> {
    ensure!(
        raise.allocations()?.iter().any(|a| a.weight > 0),
        "raise has no allocatable contributions"
    );
    if trigger == crate::presale::Trigger::CapReached {
        ensure!(
            raise.accepted_micro_usdc() == crate::presale::RAISE_CAP_MICRO_USDC,
            "cap not reached"
        );
    }
    let launch = Launch {
        mint: raise.mint,
        buyer,
        creator: raise.creator,
        name: raise.name.clone(),
        symbol: raise.symbol.clone(),
        metadata_uri: raise.metadata_uri.clone(),
        quote_asset: QuoteAsset::Sol,
        quote_budget: funded_lamports,
    };
    let plan = build(config, &launch)?;
    if trigger == crate::presale::Trigger::CapReached {
        ensure!(
            plan.quote.tokens == config.initial_reserves().2,
            "funded SOL budget does not buy the full curve"
        );
    }
    Ok((launch, plan))
}

/// Create + exact-output buy, with an explicit fee-inclusive maximum.
/// Compile this entire vector into ONE transaction: splitting it is not supported.
pub fn build(config: &Config, launch: &Launch) -> Result<LaunchPlan> {
    ensure!(
        launch.mint != launch.buyer
            && launch.mint != Pubkey::default()
            && launch.buyer != Pubkey::default(),
        "invalid mint/buyer"
    );
    for (value, max, field) in [
        (&launch.name, 32, "name"),
        (&launch.symbol, 13, "symbol"),
        (&launch.metadata_uri, 200, "metadata URI"),
    ] {
        ensure!(
            !value.is_empty() && value.len() <= max,
            "{field} exceeds supported byte length or is empty"
        );
    }
    let curve = initial_curve_for(config, launch.creator, launch.quote_asset)?;
    let quote = affordable_buy(config, &curve, launch.quote_budget)?;
    ensure!(quote.tokens > 0, "budget buys no tokens");
    let sdk = PumpSdk::new();
    let mint = pump_key(launch.mint);
    let user = pump_key(launch.buyer);
    let create = sdk.create_v2_instruction(
        mint,
        user,
        &launch.name,
        &launch.symbol,
        &launch.metadata_uri,
        pump_key(launch.creator),
        pump_key(launch.quote_asset.mint()),
        constants::SPL_TOKEN_PROGRAM_ID,
        false,
        false,
        0,
    );
    // Both official Rust and TS SDKs append QuoteControl after the three quote
    // accounts. The older creation guide lists only the first three.
    ensure!(
        create.accounts.len()
            == if launch.quote_asset == QuoteAsset::Sol {
                16
            } else {
                20
            },
        "SDK create layout changed"
    );
    // Missing trailing holder-reward argument is documented as false.
    let buy = sdk
        .buy_v2_instruction(
            &config.global,
            &curve,
            mint,
            constants::SPL_TOKEN_PROGRAM_ID,
            user,
            quote.tokens,
            launch.quote_budget,
        )
        .context("no configured Pump fee recipient")?;
    ensure!(buy.accounts.len() == 27, "SDK buy layout changed");
    let mut instructions = vec![chain_instruction(create)];
    let ata = |owner, token_mint, program| {
        chain_instruction(token::create_associated_token_account_idempotent(
            &user,
            &owner,
            &token_mint,
            &program,
        ))
    };
    instructions.push(ata(user, mint, constants::SPL_TOKEN_2022_PROGRAM_ID));
    if launch.quote_asset == QuoteAsset::Usdc {
        instructions.push(ata(
            pda::pump::bonding_curve(&mint).0,
            USDC_MINT,
            constants::SPL_TOKEN_PROGRAM_ID,
        ));
        // BUY.md requires a pre-existing buyback recipient ATA; create idempotently.
        instructions.push(ata(
            buy.accounts[8].pubkey,
            USDC_MINT,
            constants::SPL_TOKEN_PROGRAM_ID,
        ));
    }
    instructions.push(chain_instruction(buy));
    Ok(LaunchPlan {
        instructions,
        quote,
        unspent_quote: launch.quote_budget - quote.total_quote,
        token_account: chain_key(
            pda::associated_token(&user, &constants::SPL_TOKEN_2022_PROGRAM_ID, &mint).0,
        ),
        config_slot: config.slot,
    })
}
