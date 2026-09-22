# Settlement integration

The Rust settlement library is separate from the browser notification receiver
and web application. The host provides verified settled transfers, reserved
signing keys, conversion/funding, transaction delivery and processed receipts.
The web service does not currently connect those production adapters.

## Lifecycle

1. Create `presale::Raise` using the reserved mint, creator, metadata URI and
   opening Pump configuration, then persist it in `scheduler::Queue`.
2. Record unique settled payments in strictly increasing arrival order. The
   ledger uses micro-USDC, a 12,000 USDC cap and a 3,600-second deadline. Late or
   excess amounts remain explicitly unaccepted.
3. The scheduler persists a claim when the cap fills or the deadline expires.
   Empty raises expire without submitting an unfunded launch.
4. Supply the raise's actual funded lamport budget to `settlement::execute`.
   Dollar receipts or micro-USDC totals are never interpreted as lamports.
5. The executor fetches processed state and constructs an atomic Token-2022
   create, buyer ATA preparation and native SOL buy. At the cap it requires a
   full curve-tranche buyout; at timeout it computes the affordable output.
6. Verify the exact signed transaction and Pump trade receipt, then scale the
   arrival-order weights to the actual tokens received. Largest-remainder
   allocation preserves every token base unit.
7. Pack idempotent recipient ATA creation and checked-transfer pairs into V0
   transactions using measured bytes, account limits and simulation. Reconcile
   each batch's source debit and recipient credits before advancing.
8. Persist the completed receipt. Ambiguous sends require reconciliation; the
   journal blocks blind replacement attempts after crashes or timeouts.

The current regression snapshot has a 793.1-million-token sellable tranche
(79.31% of a billion). This is a historical fixture, not a hardcoded promise of
80% or the whole minted supply. Runtime planning reads actual configuration.
Creation fees, conversion costs, rent, network fees and Sender tips are separate
from the curve purchase budget.

## Construction and delivery

`launch.rs` builds instructions, `transaction.rs` compiles and signs, and
`sender.rs` owns the production broadcast entry point. Submission uses the
pinned official `helius` crate, with exact signed-byte preflight and a durable
intent journal. Read-only `examples/review.rs` cannot sign or broadcast.

Settlement enforces a **500,000 CU** limit and **2,800 micro-lamports per CU**.
The v0 priority fee is `ceil(500000 × 2800 / 1000000) = 1,400 lamports`
(0.0000014 SOL), separate from base fees, account rent and the Sender tip.
The SWQOS examples retain their 5,000-lamport tip. Limits/prices are explicit
in the signed message; the SDK never rebuilds the transaction per location.
Launch and payout transactions are simulated with that limit. Resource-limited
payout batches shrink; a launch exceeding the limit stops before broadcast.

`submit_once` no longer takes a region argument. It simulates the exact signed
bytes at processed once, reserves one journal intent, and concurrently calls
the official SDK's `send_and_confirm_via_sender` with the same immutable
transaction for every physical region in `SENDER_ENDPOINTS`: Salt Lake City,
Newark, London, Frankfurt, Amsterdam, Singapore and Tokyo. The `Default` routing
alias is not a separate physical location. Blockhash, signatures, instructions,
priority fee and tip are identical across all seven sends.
Simulation runs on the same blocking RPC client/runtime used by SDK confirmation;
it does not reuse that client's pooled connections on another Tokio runtime.

All attempts are drained, with a 30-second deadline per region, so a fast
success/error does not cancel another location's attempt. At least one region
must confirm the exact expected signature; a failed region or foreign signature
cannot count as success. `fanout.json` records every region's outcome and the
same signature/wire hash as `signed.json`. If all regions are unresolved, the
intent remains reserved for reconciliation. There is no automatic replacement
transaction or regional fee bump. A repeated intent is blocked even if the
caller supplies newly signed bytes.

The region map and tip floor were checked against the installed Helius 3.0.0
source (`optimized_transaction.rs`, `types/inner.rs`) and [Helius Sender docs](https://www.helius.dev/docs/sending-transactions/sender).
The compute instruction units were checked against `solana-compute-budget-interface`
3.0.0 and [Solana's fee formula](https://solana.com/docs/core/fees/fee-structure).

Application reads and simulations use `processed`. The pinned Helius Sender
helper internally waits for `confirmed` and does not expose a processed option.
Standard mainnet `getTransaction` cannot provide processed receipts, so a
production processed metadata stream adapter is still required. The isolated
Surfpool test uses the Helius client's local RPC adapter because public Sender
cannot deliver into a private fork. Production Sender delivery is not certified
by the fork test.

The queue and journal each have one filesystem owner. A saved in-progress claim
must be reconciled with its recorded signature and payout manifest before any
replacement is authorized. The host must provide a complete settled arrival
stream before closing a raise. Refund execution and PumpSwap migration are
outside the current integration.

## Reproduce a read-only review

```sh
cargo run --locked --example review
cargo run --locked --example review -- live
cargo run --locked --example review -- simulate \
  BUYER_PUBKEY RESERVED_MINT_PUBKEY CREATOR_PUBKEY METADATA_URI BUDGET_LAMPORTS
```

`RPC_URL` selects the RPC endpoint for live reads. `LOOKUP_TABLE` optionally
selects a verified on-chain lookup table. `VERIFY_AIRDROP_WALLET` adds a simulated
checked transfer. The simulated buyer needs existing SOL; no signing keys are
accepted by this example.

## Interface sources

Instruction layouts are checked against both the published interface and
execution evidence; regression tests retain an independent SDK comparison.
The versioned [fixture manifest](verification/manifest.json) records the exact
sources and hashes. Current runtime state must still be fetched before trading.

- [Pump IDL](https://github.com/pump-fun/pump-public-docs/blob/81091419e4457566469d4e2a27f64ed84d42419c/idl/pump.json),
  [coin creation](https://github.com/pump-fun/pump-public-docs/blob/81091419e4457566469d4e2a27f64ed84d42419c/docs/instructions/COIN_CREATION.md),
  [buy instructions](https://github.com/pump-fun/pump-public-docs/blob/81091419e4457566469d4e2a27f64ed84d42419c/docs/instructions/BUY.md).
- [Pump Rust client 0.1.13](https://docs.rs/crate/pump-rust-client/0.1.13/source/src/)
  and the independent official TypeScript SDK fixture pinned in the manifest.
- [Helius 3.0.0 Sender implementation](https://docs.rs/crate/helius/3.0.0/source/src/optimized_transaction.rs).
- [Solana simulation](https://solana.com/docs/rpc/http/simulatetransaction),
  [transaction receipts](https://solana.com/docs/rpc/http/gettransaction), and
  [Token-2022 transfer interface](https://docs.rs/crate/spl-token-2022-interface/2.1.0/source/src/instruction.rs).

See [test runners](surfpool-e2e.md) for fork and browser verification.
