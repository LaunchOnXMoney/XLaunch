# Payment notification indexer

`capture_ingest` accepts normalized X Money notifications from the local browser
collector. It does not connect to X Money itself or accept Solana transaction
payloads. Binding and storage are deployment settings:

```sh
target/debug/capture_ingest "$INDEXER_BIND_ADDRESS" "$INDEXER_DATA_DIR"
```

## Request and response

`POST /transactions`, with `Content-Type: application/json`:

```json
{
  "sender": "@alice",
  "amount": "25.50",
  "memo": "TOKEN_MINT RECIPIENT_SOLANA_WALLET"
}
```

All three fields are required; unknown fields are rejected. `sender` must be
nonempty. `amount` may be a decimal string or JSON number, positive and with at
most two decimal places. Strings make decimal preservation explicit. The memo
is stored unchanged, including Unicode. For buys, use exactly the token mint and
recipient wallet separated by whitespace, in either order. The launchpad matches
the token against its local catalog and uses the other address as the recipient. The body limit is 64 KiB.

A 200 response with `{"ok":true}` acknowledges a durable write. Validation
errors return 422; malformed JSON is rejected. Busy/storage failures return an
error rather than acknowledging an unsaved notification. `GET /healthz` reports
process availability.

This endpoint intentionally has no bearer authentication middleware. Restrict
who can reach it using your deployment network. A notification is not proof of
settled funds, and anyone with receiver access can submit its three fields.

## Durable state and ordering

The receiver writes `state.json` under the configured directory, using a
private temporary file, file fsync, atomic rename and directory fsync before
acknowledgment. Each record receives a monotonic `sequence` and server-side
`received_at`. An exclusive filesystem lock prevents two receiver processes
from owning the same state.

On restart, the receiver validates the saved version, sequence, timestamps and
payments. It refuses to reset invalid committed state. Interrupted temporary
writes are ignored. Older individual `payment-*.json` files can be imported
once when no state file exists.

The web worker consumes this state in sequence and records a content hash for
each processed receipt. Replaying the saved file does not allocate it twice.
A repeated HTTP POST, however, is a new record: there is no upstream transfer ID
in this contract. The browser collector must reconcile upstream IDs and
uncertain acknowledgments before retrying. Server receipt order is not a claim
about upstream settlement order.

See [source](../src/bin/capture_ingest.rs) and
[crash-recovery test](../tools/test_ingest_recovery.py).
