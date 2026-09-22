//! Display-only valuations. Source contracts and units: docs/market-data.md.
//! No transaction building, signing, conversion of funds, or submission occurs here.
use super::model::Coin;
use crate::{allocation, state::Config};
use anchor_lang::solana_program::program_pack::Pack;
use anchor_spl::token_2022::spl_token_2022::{extension::StateWithExtensions, state::Mint};
use anyhow::{Context, Result, ensure};
use pump_rust_client::{constants, math::fees};
use reqwest::{Client, header::HeaderMap};
use serde::{Deserialize, Serialize};
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{account::Account, pubkey::Pubkey};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

pub const BATCH_SIZE: usize = 50;
pub const REFRESH_SECONDS: i64 = 60;
pub const EXPIRE_SECONDS: i64 = 300;
const REQUEST_SPACING: u64 = 3; // 20/minute, below Jupiter's keyless 30/minute.
const PRICE_URL: &str = "https://api.jup.ag/price/v3";
pub const DEFAULT_RPC_URL: &str = "https://api.mainnet-beta.solana.com";

/// Reuse the exact fee-inclusive, arrival-ordered USDC allocation model.
/// The nonzero reserved mint stands in for the creator ONLY for this display
/// calculation: the SDK creator fee depends on zero/nonzero, not the identity.
pub fn curve_market_cap(coin: &Coin) -> Result<Option<f64>> {
    let Some(opening) = &coin.market_opening else {
        return Ok(None);
    };
    let config = Config::from_snapshot(opening)?;
    let creator = coin.mint.parse()?;
    let initial = allocation::initial_curve(&config, creator)?;
    let inputs = coin
        .buys
        .iter()
        .filter(|b| b.accepted_cents > 0)
        .map(|b| {
            Ok(allocation::Contribution {
                transfer_id: b.sequence.to_string(),
                sequence: b.sequence,
                wallet: b.wallet.parse()?,
                quote_budget: b
                    .accepted_cents
                    .checked_mul(10_000)
                    .context("quote overflow")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let allocations = allocation::allocate(&config, creator, &inputs)?;
    let mut tokens = initial.virtual_token_reserves;
    let mut quote = initial.virtual_quote_reserves;
    for buy in allocations {
        tokens = tokens
            .checked_sub(buy.weight)
            .context("token reserve underflow")?;
        quote = quote
            .checked_add(buy.curve_quote)
            .context("quote reserve overflow")?;
    }
    ensure!(tokens > 0, "empty virtual token reserve");
    let micro_usdc = fees::bonding_curve_market_cap(initial.token_total_supply, quote, tokens);
    Ok(Some(micro_usdc as f64 / 1_000_000.0))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Price {
    pub usd_price: f64,
    pub block_id: u64,
    pub decimals: u8,
}
#[derive(Clone, Copy, Debug)]
pub struct Supply {
    pub raw: u64,
    pub decimals: u8,
    pub slot: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct Quote {
    pub price_usd: f64,
    pub market_cap_usd: f64,
    pub updated_at: i64,
    pub price_block: u64,
    pub supply_slot: u64,
}
impl Quote {
    pub fn new(price: &Price, supply: Supply, now: i64) -> Result<Self> {
        ensure!(
            price.usd_price.is_finite() && price.usd_price > 0.0 && price.block_id > 0,
            "invalid Jupiter price"
        );
        ensure!(
            price.decimals == supply.decimals,
            "price/mint decimals disagree"
        );
        let market_cap_usd =
            price.usd_price * (supply.raw as f64 / 10f64.powi(i32::from(supply.decimals)));
        ensure!(
            market_cap_usd.is_finite() && market_cap_usd > 0.0,
            "invalid market cap"
        );
        Ok(Self {
            price_usd: price.usd_price,
            market_cap_usd,
            updated_at: now,
            price_block: price.block_id,
            supply_slot: supply.slot,
        })
    }
    pub fn valid(&self, now: i64) -> bool {
        now < self.updated_at + EXPIRE_SECONDS
    }
    pub fn stale(&self, now: i64) -> bool {
        now >= self.updated_at + REFRESH_SECONDS
    }
}
#[derive(Default, Serialize)]
pub struct Health {
    pub price_requests: u64,
    pub supply_requests: u64,
    pub tracked_mints: usize,
    pub last_success_at: Option<i64>,
    pub last_error: Option<String>,
    pub curve_config_slot: Option<u64>,
    pub curve_config_error: Option<String>,
}
#[derive(Default)]
pub struct Cache {
    pub quotes: BTreeMap<String, Quote>,
    pub health: Health,
}
/// Oldest-due-first rotation avoids starving the second/third batch on failure.
#[derive(Default)]
pub struct Schedule {
    due: BTreeMap<String, i64>,
}
impl Schedule {
    pub fn next(&mut self, mints: &BTreeSet<String>, now: i64) -> Vec<String> {
        self.due.retain(|mint, _| mints.contains(mint));
        for mint in mints {
            self.due.entry(mint.clone()).or_insert(0);
        }
        let mut ready: Vec<_> = self.due.iter().filter(|(_, due)| **due <= now).collect();
        ready.sort_by_key(|(mint, due)| (**due, *mint));
        let batch: Vec<_> = ready
            .into_iter()
            .take(BATCH_SIZE)
            .map(|(mint, _)| mint.clone())
            .collect();
        for mint in &batch {
            self.due.insert(mint.clone(), now + REFRESH_SECONDS);
        }
        batch
    }
}

pub struct PriceClient {
    client: Client,
    key: Option<String>,
    url: String,
    not_before: Instant,
    failures: u32,
}
impl PriceClient {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .user_agent(concat!(
                    env!("CARGO_PKG_NAME"),
                    "/",
                    env!("CARGO_PKG_VERSION")
                ))
                .timeout(Duration::from_secs(15))
                .build()?,
            key: std::env::var("JUPITER_API_KEY")
                .ok()
                .filter(|s| !s.is_empty()),
            url: PRICE_URL.into(),
            not_before: Instant::now(),
            failures: 0,
        })
    }
    pub fn ready(&self) -> bool {
        Instant::now() >= self.not_before
    }
    fn finish(&mut self, failed: bool, status: u16, headers: &HeaderMap, now: i64) {
        self.failures = if failed {
            self.failures.saturating_add(1)
        } else {
            0
        };
        let mut seconds = if failed {
            (REQUEST_SPACING * (1u64 << self.failures.min(7))).min(300)
        } else {
            REQUEST_SPACING
        };
        if status == 401 || status == 403 {
            seconds = seconds.max(60);
        }
        let remaining = headers
            .get("x-ratelimit-remaining")
            .and_then(|s| s.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok());
        if (status == 429 || remaining.is_some_and(|n| n <= 1))
            && let Some(reset) = headers
                .get("x-ratelimit-reset")
                .and_then(|s| s.to_str().ok())
                .and_then(|s| s.parse::<i64>().ok())
        {
            seconds = seconds.max(reset.saturating_sub(now).max(0) as u64 + 1);
        }
        self.not_before = Instant::now() + Duration::from_secs(seconds);
    }
    pub async fn prices(&mut self, mints: &[String]) -> Result<BTreeMap<String, Option<Price>>> {
        ensure!(
            !mints.is_empty() && mints.len() <= BATCH_SIZE,
            "Jupiter batch must contain 1..=50 mints"
        );
        ensure!(
            self.ready(),
            "Jupiter request attempted before pacing deadline"
        );
        let mut request = self
            .client
            .get(&self.url)
            .query(&[("ids", mints.join(","))]);
        if let Some(key) = &self.key {
            request = request.header("x-api-key", key);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                self.finish(true, 0, &HeaderMap::new(), chrono::Utc::now().timestamp());
                return Err(error.without_url().into());
            }
        };
        let status = response.status();
        let headers = response.headers().clone();
        let result = if status.is_success() {
            response
                .json::<BTreeMap<String, Option<Price>>>()
                .await
                .map_err(|e| anyhow::anyhow!(e.without_url()))
        } else {
            Err(anyhow::anyhow!("Jupiter HTTP {}", status.as_u16()))
        };
        self.finish(
            result.is_err(),
            status.as_u16(),
            &headers,
            chrono::Utc::now().timestamp(),
        );
        result
    }
}

pub fn supply(account: &Account, slot: u64) -> Result<Supply> {
    ensure!(!account.executable, "executable mint account");
    let (raw, decimals) = if account.owner == crate::chain_key(constants::SPL_TOKEN_2022_PROGRAM_ID)
    {
        let mint = StateWithExtensions::<Mint>::unpack(&account.data)?;
        (mint.base.supply, mint.base.decimals)
    } else {
        ensure!(
            account.owner == crate::chain_key(constants::SPL_TOKEN_PROGRAM_ID),
            "invalid mint owner"
        );
        let mint = anchor_spl::token::spl_token::state::Mint::unpack(&account.data)?;
        (mint.supply, mint.decimals)
    };
    Ok(Supply {
        raw,
        decimals,
        slot,
    })
}
/// One processed RPC batch, never one call per mint or per browser request.
pub fn supplies(rpc_url: &str, mints: &[String]) -> Result<BTreeMap<String, Supply>> {
    ensure!(
        !mints.is_empty() && mints.len() <= BATCH_SIZE,
        "invalid supply batch"
    );
    let keys = mints
        .iter()
        .map(|m| m.parse::<Pubkey>())
        .collect::<Result<Vec<_>, _>>()?;
    let helius = helius::Helius::new_with_url(rpc_url)?;
    let response = helius
        .rpc_client
        .solana_client
        .get_multiple_accounts_with_commitment(&keys, CommitmentConfig::processed())
        .map_err(|_| anyhow::anyhow!("mint supply RPC failed"))?;
    ensure!(response.value.len() == mints.len(), "incomplete mint batch");
    let mut result = BTreeMap::new();
    for (mint, account) in mints.iter().zip(response.value) {
        // A missing/invalid account removes only this mint's valuation.
        if let Some(account) = account
            && let Ok(supply) = supply(&account, response.context.slot)
        {
            result.insert(mint.clone(), supply);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        extract::{Query, State},
        http::StatusCode,
        response::IntoResponse,
        routing::get,
    };
    use std::sync::{Arc, Mutex};

    #[test]
    fn batching_rotates_all_101_mints_without_starvation_or_duplicates() {
        let mints: BTreeSet<_> = (0..101).map(|i| format!("mint-{i:03}")).collect();
        let mut schedule = Schedule::default();
        let a = schedule.next(&mints, 100);
        let b = schedule.next(&mints, 103);
        let c = schedule.next(&mints, 106);
        assert_eq!((a.len(), b.len(), c.len()), (50, 50, 1));
        assert_eq!(
            a.iter()
                .chain(&b)
                .chain(&c)
                .cloned()
                .collect::<BTreeSet<_>>(),
            mints
        );
        assert!(schedule.next(&mints, 159).is_empty());
        assert_eq!(schedule.next(&mints, 160), a);
        assert_eq!(schedule.next(&mints, 163), b);
        assert!(schedule.next(&BTreeSet::new(), 200).is_empty());
    }

    #[test]
    fn valuation_uses_actual_supply_decimals_and_expires() -> Result<()> {
        let mut price = Price {
            usd_price: 0.0001,
            decimals: 6,
            block_id: 123,
        };
        let supply = Supply {
            raw: 800_000_000_000_000,
            decimals: 6,
            slot: 124,
        };
        let quote = Quote::new(&price, supply, 100)?;
        assert_eq!(quote.market_cap_usd, 80_000.0); // NOT assumed 1bn supply.
        assert!(!quote.stale(159));
        assert!(quote.stale(160));
        assert!(quote.valid(399));
        assert!(!quote.valid(400));
        price.decimals = 9;
        assert!(Quote::new(&price, supply, 100).is_err());
        price.decimals = 6;
        price.usd_price = f64::INFINITY;
        assert!(Quote::new(&price, supply, 100).is_err());
        price.usd_price = -1.0;
        assert!(Quote::new(&price, supply, 100).is_err());
        Ok(())
    }

    #[test]
    fn sliding_window_headers_and_errors_set_global_backoff() -> Result<()> {
        let mut client = PriceClient::new()?;
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-reset", "160".parse()?);
        headers.insert("x-ratelimit-remaining", "0".parse()?);
        client.finish(true, 429, &headers, 100);
        assert!(client.not_before.duration_since(Instant::now()).as_secs() >= 60);
        assert!(!client.ready());
        client.finish(false, 200, &headers, 100);
        assert!(client.not_before.duration_since(Instant::now()).as_secs() >= 60);
        client.finish(true, 503, &HeaderMap::new(), 100);
        let first = client.not_before.duration_since(Instant::now());
        client.finish(true, 503, &HeaderMap::new(), 100);
        assert!(client.not_before.duration_since(Instant::now()) > first);
        Ok(())
    }

    #[tokio::test]
    async fn real_http_client_batches_and_handles_missing_prices_and_429() -> Result<()> {
        type Requests = Arc<Mutex<Vec<Vec<String>>>>;
        async fn handler(
            State(requests): State<Requests>,
            Query(query): Query<BTreeMap<String, String>>,
        ) -> axum::response::Response {
            let ids: Vec<String> = query["ids"].split(',').map(String::from).collect();
            requests.lock().unwrap().push(ids.clone());
            if requests.lock().unwrap().len() == 2 {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(
                        "x-ratelimit-reset",
                        (chrono::Utc::now().timestamp() + 65).to_string(),
                    )],
                    "rate limited",
                )
                    .into_response();
            }
            axum::Json(
                serde_json::json!({ids[0].clone(): {"usdPrice":0.0001,"decimals":6,"blockId":123}}),
            )
            .into_response()
        }
        let requests: Requests = Arc::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let router = Router::new()
            .route("/price/v3", get(handler))
            .with_state(requests.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let mut client = PriceClient::new()?;
        client.url = format!("http://{address}/price/v3");
        client.key = None;
        let mints: Vec<_> = (0..50)
            .map(|i| Pubkey::new_from_array([i + 1; 32]).to_string())
            .collect();
        let response = client.prices(&mints).await?;
        assert_eq!(response.len(), 1);
        assert!(!response.contains_key(&mints[1]));
        assert_eq!(requests.lock().unwrap()[0], mints);
        assert!(client.prices(&mints).await.is_err()); // No HTTP while gate is closed.
        assert_eq!(requests.lock().unwrap().len(), 1);
        client.not_before = Instant::now();
        assert!(
            client
                .prices(&mints)
                .await
                .unwrap_err()
                .to_string()
                .contains("429")
        );
        assert!(client.not_before.duration_since(Instant::now()).as_secs() >= 64);
        client.not_before = Instant::now();
        assert!(client.prices(&vec![mints[0].clone(); 51]).await.is_err());
        assert!(client.prices(&[]).await.is_err());
        assert_eq!(requests.lock().unwrap().len(), 2);
        task.abort();
        Ok(())
    }

    #[tokio::test]
    #[ignore = "read-only live Jupiter and mainnet RPC verification"]
    async fn live_price_and_processed_supply_and_curve_config() -> Result<()> {
        let mint = Config::usdc_mint().to_string();
        let mut client = PriceClient::new()?;
        let prices = client.prices(std::slice::from_ref(&mint)).await?;
        let mint_copy = mint.clone();
        let (supplies, config) = tokio::task::spawn_blocking(move || -> Result<_> {
            let rpc = std::env::var("RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.into());
            let supplies = supplies(&rpc, &[mint_copy])?;
            let helius = helius::Helius::new_with_url(&rpc)?;
            Ok((supplies, Config::fetch(&helius)?))
        })
        .await??;
        let quote = Quote::new(
            prices[&mint].as_ref().context("no live price")?,
            supplies[&mint],
            chrono::Utc::now().timestamp(),
        )?;
        let curve = allocation::initial_curve(&config, Config::usdc_mint())?;
        println!(
            "Verified keyless Jupiter and processed mint supply: {}",
            serde_json::to_string(&quote)?
        );
        println!(
            "Verified live Pump configuration slot {}, initial curve market cap = ${:.6}",
            config.slot,
            fees::bonding_curve_market_cap(
                curve.token_total_supply,
                curve.virtual_quote_reserves,
                curve.virtual_token_reserves
            ) as f64
                / 1_000_000.0
        );
        Ok(())
    }
}
