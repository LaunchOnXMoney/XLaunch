# Market caps and the Explore page

`/` is the landing page; `/explore` contains token search, tabs, sorting, and
pagination. Both Raising and Graduated cards show market cap rather than raised
dollars. The progress ring still represents accepted dollars / the raise cap.

## Raising valuation

`Curve market cap` is an estimate from the local USD/USDC-denominated presale
model. It is not an observed exchange price or a promise of the eventual SOL
launch valuation. No spot-price conversion of contributions or funds happens.

The background worker reads Pump Global, FeeConfig, and the USDC mint together
at **processed** commitment. The existing strict decoder checks owners, layouts,
reserves, supply, quote decimals, and fee tiers. Each new coin saves a copy in
`catalog.json` as `market_opening`, before subsequent buys are projected. Existing
unfunded coins can acquire a snapshot; funded legacy coins without one remain
unpriced because their opening parameters are unknown. Snapshot writes use the
same atomic write/fsync mechanism as the rest of the catalog. Refreshing the
current configuration never changes an existing coin's opening snapshot.
A newly observed coin can use configuration fetched within the last 120 seconds;
configuration is refreshed every minute. An outage leaves new coins unpriced
until verified parameters are available before their first accepted contribution.

Accepted cents become micro-USDC (`cents × 10,000`). Replay uses the existing
integer, fee-inclusive allocator in receiver sequence order; late/unallocated
amounts do not affect the curve. Only the buy's net curve quote increases virtual
quote reserves; protocol/creator fees do not. Purchased tokens decrease virtual
token reserves. The displayed total-supply valuation is:

```
market_cap_micro_USDC = floor(virtual_quote_reserves × total_supply / virtual_token_reserves)
market_cap_USD = market_cap_micro_USDC / 1,000,000
```

The display uses the ordinary nonzero-creator fee model. The SDK's
`creator_fee_amount` depends only on whether the creator is zero; no creator
identity is inferred from an X handle. This calculation shares `allocate` and
`affordable_buy` with the allocation library but does not connect the web
receiver to production settlement.

## Graduated valuation and caching

A completed distribution receipt is required for the existing `graduated`
status. One background worker queries Jupiter Price V3 in groups of **at most
50 unique mints**, oldest due first. It issues requests at least three seconds
apart (at most 20/minute) and aims to refresh each mint once per minute. More than
50 coins cause additional paced requests, never one oversized request or one
request per card. Very large sets take longer than a minute to cycle. Empty
sets make no price requests. `JUPITER_API_KEY` is optional and remains server-side;
adding one does not increase this conservative default pace.

Each returned-price batch is paired with one **processed** `getMultipleAccounts`
RPC call for mint supplies. Supported SPL Token/Token-2022 mint accounts are
decoded and their decimals must match Jupiter. The valuation is:

```
market_cap_USD = usdPrice × (raw_mint_supply / 10^mint_decimals)
```

This is a total minted supply valuation, not a separately estimated circulating
supply. It does not assume every mint still has one billion tokens after burns.
Jupiter's `priceChange24h` is not volume and is not presented as volume.

A successful response that omits a mint (or returns null) removes its cached
price immediately. Failed requests retain the last good quote, mark it stale
after 60 seconds, and stop displaying it after five minutes. `price_updated_at`
is the last successful check time, not the time of a trade. `price_block` exposes
Jupiter's source block; regressing price blocks or RPC supply slots do not refresh
a cache entry. Missing/invalid prices display `—`, never a fabricated zero.

HTTP 429 and network/server failures trigger global exponential backoff. The
worker also honors Jupiter's `x-ratelimit-reset` Unix timestamp when a request is
rate limited or the remaining window count is at most one, including on HTTP
200. Authentication errors wait at least a minute. Failed batches rotate behind
other due batches. There is no retry burst on recovery.

All visitors share one in-memory snapshot and bounded page cache, with ETags.
List/detail HTTP handlers never fetch external data. Graduated quotes are
refetched after restart; durable curve snapshots retain raising valuations.
`/api/health` includes counters, latest configuration slot, and pricing errors.
Run one web worker per Jupiter rate-limit bucket or divide that budget across
instances; the in-process limiter does not coordinate multiple deployments.

## Sources and verification

- [Jupiter Price V3](https://developers.jup.ag/docs/price): endpoint, comma-separated
  ids, 50-mint limit, response fields, omitted/unreliable prices.
- [Jupiter rate limits](https://developers.jup.ag/docs/portal/rate-limits): keyless
  30/minute, organisation-level sliding window, response headers and HTTP 429.
- The installed `pump-rust-client` 0.1.13 source, `src/math/fees.rs`:
  `bonding_curve_market_cap` and `creator_fee_amount`; `src/math/bonding_curve.rs`
  supplies the independent quote check used by our allocator.
- [Existing Pump evidence](rust-settlement.md#interface-sources) and `src/state.rs`:
  published program layouts cross-checked against live processed mainnet state.
- Installed `solana-rpc-client` 4.2.1, `get_multiple_accounts_with_commitment`, and
  `spl-token-2022-interface` 2.1.0, `StateWithExtensions<Mint>::unpack` / `Mint`:
  actual RPC signature and raw supply/decimals layout.

`cargo test --lib --test web` checks ordered curve math, restart persistence,
actual-supply normalization, expiration, 101-mint rotation, and an HTTP fixture
that exercises the real client, omitted prices and 429/backoff. The opt-in
read-only live test checks Jupiter plus mainnet mint supplies and Pump config:

```sh
cargo test --lib live_price_and_processed_supply_and_curve_config -- --ignored --nocapture
```

The browser/API test additionally verifies separate navigation, no token requests
from Home, visible curve market cap, and cached API reads without additional
Jupiter or mint-supply requests. It uses real Pinata uploads and read-only RPC;
it sends no Solana transaction.
