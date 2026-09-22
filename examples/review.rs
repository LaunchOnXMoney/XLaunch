//! Read-only review. No signing or submission is reachable from this example.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use helius::Helius;
use serde_json::{Value, json};
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{hash::Hash, pubkey::Pubkey};
use xlaunch::{
    launch::{self, Launch},
    sender::RpcSimulator,
    state::Config,
    transaction::{self, Envelope, SenderTier},
};

fn main() -> Result<()> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        arguments.is_empty()
            || arguments == ["live"]
            || (arguments.len() == 6 && arguments[0] == "simulate"),
        "usage: review [live | simulate BUYER_PUBKEY RESERVED_MINT_PUBKEY CREATOR_PUBKEY METADATA_URI BUDGET_LAMPORTS]"
    );
    let reference: Value =
        serde_json::from_str(include_str!("../docs/verification/sdk-reference.json"))?;
    let live = !arguments.is_empty();
    let helius = if live {
        Some(Helius::new_with_url(
            &std::env::var("RPC_URL").unwrap_or_else(|_| "https://api.mainnet.solana.com".into()),
        )?)
    } else {
        None
    };
    let config = if let Some(h) = &helius {
        Config::fetch(h)?
    } else {
        Config::saved_evidence()?
    };
    let simulate = arguments.first().is_some_and(|v| v == "simulate");
    let launch = if simulate {
        Launch {
            buyer: arguments[1].parse()?,
            mint: arguments[2].parse()?,
            creator: arguments[3].parse()?,
            name: "Verification".into(),
            symbol: "VERIFY".into(),
            metadata_uri: arguments[4].clone(),
            quote_asset: xlaunch::state::QuoteAsset::Sol,
            quote_budget: arguments[5].parse()?,
        }
    } else {
        Launch {
            buyer: reference["user"].as_str().unwrap().parse()?,
            mint: reference["mint"].as_str().unwrap().parse()?,
            creator: reference["creator"].as_str().unwrap().parse()?,
            name: "Verification".into(),
            symbol: "VERIFY".into(),
            metadata_uri: "https://example.com/token.json".into(),
            quote_asset: xlaunch::state::QuoteAsset::Sol,
            quote_budget: 100_000_000,
        }
    };
    let plan = launch::build(&config, &launch)?;
    let mut envelope = Envelope {
        payer: launch.buyer,
        blockhash: Hash::default(),
        last_valid_block_height: 1,
        lookup_tables: vec![],
        compute_unit_limit: transaction::SETTLEMENT_COMPUTE_UNIT_LIMIT,
        compute_unit_price: transaction::SETTLEMENT_COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
        tip_lamports: 5000,
        tier: SenderTier::Swqos,
    };
    let without_alt = transaction::wire_size(&envelope.compile(&plan.instructions)?)?;
    if let Some(h) = &helius {
        let alt = std::env::var("LOOKUP_TABLE")
            .unwrap_or_else(|_| pump_rust_client::constants::MAINNET_ALT.to_string())
            .parse::<Pubkey>()?;
        envelope
            .lookup_tables
            .push(transaction::fetch_lookup_table(h, alt)?);
        (envelope.blockhash, envelope.last_valid_block_height) = h
            .rpc_client
            .solana_client
            .get_latest_blockhash_with_commitment(CommitmentConfig::processed())?;
    }
    let transaction = envelope.compile(&plan.instructions)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "scope": if live { "live read-only review; not executed" } else { "historical snapshots; not executed" },
            "config_slot":config.slot, "mint":launch.mint.to_string(), "quote_asset":launch.quote_asset,
            "quote_mint":launch.quote_asset.mint().to_string(), "quote_budget_lamports":launch.quote_budget,
            "base_tokens":plan.quote.tokens, "net_curve_quote":plan.quote.curve_quote, "fee_inclusive_quote":plan.quote.total_quote,
            "unspent_quote":plan.unspent_quote, "body_instructions":plan.instructions.len(), "wire_bytes_without_alt":without_alt,
            "wire_bytes":transaction::wire_size(&transaction)?, "fits_v0_limits":transaction::fits(&transaction)?,
            "required_signers":transaction.signatures.len(), "submission":false,
            "unsigned_transaction_base64":STANDARD.encode(bincode::serialize(&transaction)?)
        }))?
    );
    if simulate {
        ensure!(
            transaction::fits(&transaction)?,
            "launch does not fit; use a verified custom lookup table containing additional launch accounts"
        );
        let simulator = RpcSimulator {
            helius: helius.as_ref().unwrap(),
            min_context_slot: config.slot,
        };
        let response = simulator.simulate_raw_with_accounts(&transaction, &[launch.mint])?;
        println!("{}", serde_json::to_string_pretty(&response)?);
        ensure!(
            response.value.err.is_none(),
            "simulation failed: {:?}",
            response.value.err
        );
        if let Ok(recipient) = std::env::var("VERIFY_AIRDROP_WALLET") {
            let recipient: Pubkey = recipient.parse()?;
            let mint_account = response
                .value
                .accounts
                .as_ref()
                .and_then(|a| a.first())
                .and_then(Option::as_ref)
                .and_then(|a| a.to_account())
                .context("simulated mint account missing")?;
            let distribution = xlaunch::distribution::Distribution::new(
                launch.mint,
                &mint_account,
                launch.buyer,
                launch.buyer,
                vec![xlaunch::allocation::Payout {
                    wallet: recipient,
                    amount: plan.quote.tokens,
                }],
            )?;
            // Simulation state is discarded by RPC, so replay the creation/buy
            // before testing a transfer of the resulting mint. No state is written.
            let mut body = plan.instructions.clone();
            body.extend(distribution.all_instructions());
            let tx = envelope.compile(&body)?;
            ensure!(
                transaction::fits(&tx)?,
                "combined verification transaction does not fit"
            );
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"stage":"create_buy_and_airdrop_simulation", "wire_bytes":transaction::wire_size(&tx)?, "recipient":recipient.to_string(), "amount":plan.quote.tokens,
                "unsigned_transaction_base64":STANDARD.encode(bincode::serialize(&tx)?)})
                )?
            );
            let transferred = simulator.simulate_raw(&tx)?;
            println!("{}", serde_json::to_string_pretty(&transferred)?);
            ensure!(
                transferred.value.err.is_none(),
                "airdrop simulation failed: {:?}",
                transferred.value.err
            );
        }
    }
    Ok(())
}
