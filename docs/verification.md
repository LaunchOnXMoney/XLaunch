# Public regression fixtures

`docs/verification/` contains historical public Pump IDLs, on-chain account
snapshots, an independent SDK instruction/quote oracle, and unsigned simulation
results used by `src/state.rs`, `tests/settlement.rs` and `examples/review.rs`.
These files are intentional test inputs. They contain public protocol data,
not operator signing keys, browser sessions or payment records.

The [manifest](verification/manifest.json) records source URLs, the pinned Pump
documentation revision, snapshot slots, dependency versions and SHA-256 hashes.
The fixture's original `finalized` collection commitment is retained as
provenance; runtime reads and simulations use `processed`.

```sh
python3 tools/verify_snapshot.py
```

The verifier checks hashes, RPC errors, account ownership, IDL discriminators,
full Global layout, the quote-mint whitelist and mint precision, then prints a
historical reserve calculation. It performs no network requests or transactions.
The Rust suite independently compares SDK instruction bytes, fee calculations
and simulated token deltas against these fixtures.

Snapshot values are not current configuration, a live quote, or a guaranteed
funding amount. Runtime trading fetches deployed state. New signed fork runs,
web screenshots, payment logs and private deployment diagnostics belong under
ignored `var/`; they are not committed as historical status reports.

The unsigned simulations used a public Pump fee-recipient address as the
simulated buyer with signature verification disabled. No private key for that
account was held or used, and those simulations were not broadcasts. See
[settlement](rust-settlement.md) for the production integration boundaries and
[test runners](surfpool-e2e.md) for reproducible execution checks.
