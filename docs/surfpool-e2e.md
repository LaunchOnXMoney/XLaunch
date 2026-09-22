# Verification runners

## Offline checks

```sh
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
python3 tools/verify_snapshot.py
cargo build --locked --bins
python3 tools/test_ingest_recovery.py
```

The Rust suite covers instruction/quote fixtures, ordered allocation, cap and
deadline boundaries, payout conservation, transaction packing, receipts,
journaling, notification recovery, memo parsing, and prepared metadata.
`tests/sender_fanout.rs` exercises the actual Helius SDK through a loopback proxy
in a child process. All seven regions must reach a barrier before any responds,
proving concurrency. It compares every transmitted Base64 payload with the exact
simulated and journaled bytes, checks one POST per location, handles partial and
complete regional failures, rejects a foreign returned signature, blocks
replacement intents and sends nothing after failed simulation. It contacts no
public Sender endpoint.

The receiver test uses a temporary loopback listener and verifies acknowledged
writes through SIGKILL and restart. It does not contact X Money.

## Signed mainnet fork

Install Surfpool 1.5.0, then run:

```sh
tools/test_surfpool.sh
```

The runner starts a fresh loopback mainnet fork, verifies its version, and stops
only its own child process. It rejects occupied test listeners. Override
`XLAUNCH_SURFPOOL_PORT`, `XLAUNCH_SURFPOOL_WS_PORT`, and
`XLAUNCH_SURFPOOL_STUDIO_PORT` if needed. These are test settings, unrelated to
production listeners. Generated logs, signatures and evidence stay under
ignored `var/surfpool-e2e/`.

For a separately started, isolated fork, set `XLAUNCH_SURFPOOL_URL` and run:

```sh
cargo test --locked --test surfpool_e2e -- --ignored --nocapture
```

The harness rejects non-loopback endpoints and unverified Surfpool versions.
It generates ephemeral keys, funds the isolated payer through fork cheatcodes,
and executes imported programs with real signature and blockhash checks.
It covers atomic rollback, partial and complete buys, cap/deadline settlement,
maximal complete-recipient batches, exact final balances, and duplicate-intent
protection. Test-funded SOL budgets are fixtures, not quoted USDC/SOL exchange
rates. Application RPC calls use `processed`. All signed test envelopes use
500,000 CU and 2,800 micro-lamports/CU. The tests execute create/buy and all
payouts under this limit; the two compute instructions are also verified against
the SDK's encoder in the offline suite. Surfpool's recorded `meta.fee` has
reported base fees only, so the fork is not evidence that mainnet priority fees
were charged; the fee amount is checked from the encoded instructions and the
published v0 fee formula.

The fork test uses Helius's embedded local RPC because public Sender cannot
reach the fork. It does not submit to mainnet or validate production conversion,
X Money settlement, custody, or processed receipt streaming.

## Pinata and browser E2E

Use Node 20+, Python 3 with Pillow, and the Pinata variables from your private
environment. Install the locked Playwright dependency and its Chromium:

```sh
npm ci
npx playwright install chromium
python3 -m pip install Pillow
cargo build --locked --bin launchpad
python3 tools/test_web_e2e.py
```

The script reads `PINATA_JWT` and `IPFS_GATEWAY` from the environment; it does
not search another project or load production credential files. An optional
`PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH` selects an existing Chromium installation.
It starts its own backend and payment fixture on loopback, performs real Pinata
uploads, reads metadata back through the configured gateway, checks cached
responses and avatars, kills/restarts the backend, and exercises actual browser
copy/paste and flattened launch-note ingestion. Reports and screenshots are
written under that run's ignored `var/web-e2e/` directory.

For clipboard checks against an explicitly selected HTTP test deployment,
set `WEB_TEST_URL`, `WEB_TEST_IMAGE` and `WEB_TEST_OUT`, then run
`node tools/test_clipboard_http.cjs`. It creates test image/metadata uploads.
No test defaults to a production host.

References: [Surfpool cheatcodes](https://solana.com/docs/tools/surfpool/rpc/cheatcodes),
[versioned Surfpool source](https://github.com/solana-foundation/surfpool/tree/86493c4bf716b01d4bedbc531aa9288cb14d5f36),
[Playwright library](https://playwright.dev/docs/library).
