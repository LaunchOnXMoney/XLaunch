# Browser relay and optional capture

## Data source

XLaunch sources X Money activity through an authenticated browser instance
controlled by the operator. The browser-side collector performs activity
requests in that session, extracts the payment fields, and forwards normalized
notifications to the Rust indexer. The collector runs separately from this
repository; private X Money routes and session credentials are not included.

The browser relay addresses the rate limiting and automated-access restrictions
reported during direct polling. These are more precise terms than “bans”:
[rate limiting](https://docs.x.com/x-api/fundamentals/rate-limits) and
[locked or restricted accounts](https://help.x.com/en/managing-your-account/locked-and-limited-accounts)
are different conditions. Those links describe X generally, not a published
X Money transfer API or verified Money-specific thresholds.

Requests originate through the browser session, and only `sender`, `amount`
and `memo` are posted to `/transactions`. This is application-level relaying,
not a server-wide network proxy. Cookies, login tokens, request headers, and
raw response bodies do not belong in that POST. Browser sessions remain subject
to upstream limits and authentication challenges; the collector must back off
or pause when access is restricted.

The [receiver contract](capture-ingest.md) defines exact amounts, ordering and
acknowledgments. The collector must track upstream transfer identity and avoid
resending already acknowledged payments. The receiver's current three-field
format cannot distinguish a retry from another payment with the same fields.

## Optional Kameleo recorder

`kameleo_capture` is a passive Rust Chrome DevTools Protocol recorder for one
explicitly selected Kameleo profile. It is a diagnostic tool, separate from the
payment relay: it does not parse X Money payments or post notifications.

Configure the profile's CDP WebSocket URL privately, using Kameleo's
[documented integration](https://developer.kameleo.io/integrations/playwright/).
Keep the browser and Kameleo Engine on the operator's machine; make that endpoint
reachable only through your private connection when recording remotely.

```sh
cargo build --locked --bin kameleo_capture
mkdir -p var/captures
target/debug/kameleo_capture "$KAMELEO_CDP_URL" "$CAPTURE_OUTPUT_DIR"
```

`CAPTURE_OUTPUT_DIR` must be a new directory. The recorder follows page, iframe
and worker targets and writes:

| File | Content |
| --- | --- |
| `capture.har` | HAR snapshot, atomically replaced every five seconds |
| `events.jsonl` | Raw CDP events, extra headers and WebSocket frames |
| `requests.jsonl` | Method, origin, path and resource type index |
| `ready` | Marker indicating network recording is enabled |

Captures can contain session credentials and payment data. They belong in
private storage under ignored `var/`, never in a public issue or commit. Output
directories use mode 0700 and raw files use 0600. Stop with SIGINT or SIGTERM
to flush the current capture. A disconnected CDP session ends recording; start
a fresh output directory after reconnecting.

This records traffic observed after attachment. It cannot recover earlier
traffic, and Chrome may withhold redirect, evicted, detached or streaming
response bodies. Missing bodies are marked explicitly. HAR contains ordinary
headers; extra-info events and WebSocket frames stay in `events.jsonl`.
Multipart file content is not reconstructed from `getRequestPostData`.

Implementation references:
[CDP protocol definitions](https://github.com/ChromeDevTools/devtools-protocol/tree/master/json),
[HAR 1.2](https://github.com/ahmadnassri/har-spec/blob/master/versions/1.2.md),
and [recorder source](../src/bin/kameleo_capture.rs).
