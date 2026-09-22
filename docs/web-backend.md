# Web application and cached API

The `launchpad` binary serves the uploaded HTML templates with live adapters
from `frontend-api/`. It reads receiver state in a background worker and keeps
public responses in memory. Runtime directories and environment files are
private deployment inputs.

```sh
target/debug/launchpad "$WEB_BIND_ADDRESS" \
  "$INDEXER_DATA_DIR/state.json" "$LAUNCHPAD_DATA_DIR" \
  frontend frontend-api frontend-vendor
```

The receiver must initialize its state file before the website starts.
`PINATA_JWT` and an HTTPS `IPFS_GATEWAY` prefix ending in `/` are required.
Optional `X_MONEY_HANDLE` and `X_MONEY_URL` configure the displayed receiving
account and payment link, and optional `GITHUB_URL` sets the header's GitHub
link; omit them when unset. Every optional public link must be an HTTPS URL
without embedded credentials, since it is published to every visitor. `/go/x-money` returns 503 until
the destination is configured.

## Routes

| Route | Purpose |
| --- | --- |
| `GET /` | Landing page with launch count and receiving handle |
| `GET /explore` | Separate searchable token browser |
| `GET /deploy` | Launch configuration page |
| `GET /how-it-works` | Illustrated four-step explanation and current terms |
| `GET /api/tokens` | Paginated, searchable and sorted token list |
| `GET /api/tokens/{mint}` | Token and ordered contributions |
| `GET /api/tokens/{mint}/metadata` | Public token metadata |
| `GET /api/stats` | Raise/graduation counts and accepted totals |
| `GET /api/config` | Fee, cap, duration, receiving handle and payment URL |
| `GET /api/health`, `GET /healthz` | Projection health and cache counters |
| `POST /api/images` | Raw square PNG/JPEG upload |
| `POST /api/buy-note` | Build and prove the note a buyer must send |
| `POST /api/launch-config/validate` | Validate a launch configuration |
| `POST /api/launch-config/prepare` | Pin metadata and durably save its reference |
| `GET /go/x-money` | Configured payment redirect |
| `GET /media/{filename}` | Registered, cached uploaded images |

Token-list parameters: `status=raising|graduated`,
`sort=marketcap|raised|recent|backers|fdv|volume24h`, `q`, `page`, and `per_page` (1–60).
Defaults match the frontend: Graduated, Market cap, page 1, six cards.
`market_cap_usd` is the headline value; `fdv_usd` is a compatibility alias.
Unavailable market caps and volumes remain null. Jupiter does not supply volume,
so the UI offers Market cap, Recent, and Backers sorting. A raise awaiting deployment remains in Raising.

The site is branded X Launch. Its mark is the money bag, defined once per page
as the `money-bag` SVG symbol and referenced everywhere else. The supplied artwork is a dark bag
with light detail. On the dark page the mark is drawn light, which inverts that
tone; an earlier revision put it on a light disc to preserve the original tone,
and that disc was removed by request.

The home page is a single centred hero: the receiving handle from `/api/config`
as a click-to-copy pill (falling back to `@LaunchOnXMoney` when
`X_MONEY_HANDLE` is unset; that fallback is defined once, in
`window.xlaunchSiteLinks`), the `total` count from `/api/stats` as the Launches figure, and a Launch
Coin button in the top right that opens the Deploy page. An earlier revision of
this document said the token list sits below that hero under an `#explore`
anchor. That is no longer true: the list lives on its own Explore page.

The X bird mark is kept only where it means X itself: the header X and GitHub
icons, the per-token social links, and the Deploy page's Open X Money payment
button. Each header icon renders only when its link is configured, so an
unconfigured deployment shows neither. Both links come from
`window.xlaunchSiteLinks` in `frontend-api/runtime-config.js`, which is the
single place that derives them from `/api/config`; page adapters must not build
them again.

That shared runtime is inlined into every page rather than linked. An earlier
revision served it at `/runtime-config.js`, and that was wrong: pages are sent
`no-cache` while a linked script was cached for an hour, so a returning visitor
ran fresh adapter code against a stale runtime and the page died with
`window.xlaunchSiteLinks is not a function`. Anything the adapters call at
render time therefore ships inside the page, not beside it.

The header is pinned to the top of every page and must not move between pages.
Two rules keep it still. It is `position:sticky`, so scrolling does not carry it
away, and `html` sets `scrollbar-gutter:stable` with `overflow-y:scroll`, so the
scrollbar track is always reserved. Without that second rule the centred
container is fifteen pixels wider on pages short enough to avoid a scrollbar,
and the header visibly jumps on every navigation between a scrolling page and a
short one. Measure the header's position on all four pages after changing page
height, not just one.

The How It Works page explains the flow in four steps and states the current
terms. Its raise target, time limit and creation fee are read from
`/api/config`, so they follow the backend rather than repeating its numbers.

## Buying into a raise

Selecting a coin on the Explore page opens a modal that takes the buyer's Solana
address and returns the note to attach to their X Money payment. The note is two
whitespace-separated addresses, the token and the wallet, and the indexer tells
them apart by catalog membership rather than by position.

The page never assembles that note. It posts the pair to `/api/buy-note`, which
builds the note and then parses it back with the same function the indexer runs
on arrival, confirming it resolves to exactly the requested token and wallet. An
address that is itself a listed token is refused there, because such a note would
be ambiguous on payment. So a note the page can display is one the indexer will
accept.

## Metadata and launch memo

Upload image bytes to `/api/images`. Images must be square, under 2 MiB, and
at most 4096×4096. The response contains `cid`, `uri`, `url` and `preview_url`.
Then POST this configuration to `/api/launch-config/prepare`:

```json
{
  "name": "Example Coin",
  "symbol": "EXAMPLE",
  "image_uri": "ipfs://IMAGE_CID",
  "socials": {
    "website": "https://example.com",
    "twitter": "https://x.com/example",
    "telegram": "https://t.me/example"
  }
}
```

The metadata includes name, symbol, image and optional social fields. Its JSON
is uploaded through Pinata's [V3 file API](https://docs.pinata.cloud/api-reference/endpoint/upload-a-file).
The preparation response contains `uri`, `cid`, `url` and `metadata`; `uri`
points to JSON, distinct from the image URI. The mapping is saved before the
response, so a restart between copying the note and receiving payment is safe.

The frontend emits only:

```text
Name: Example Coin
Symbol: EXAMPLE
Metadata Uri: ipfs://METADATA_CID
```

The indexer resolves that URI against prepared metadata and checks the name
and symbol. Socials do not need to fit in the memo. The parser accepts whitespace
between labels, including notes whose line breaks were flattened by transport,
and rejects duplicate labels. The legacy `Image Uri` label is accepted for
previously prepared notes; new notes use `Metadata Uri`.

The exact current creation fee is read from `/api/config` (currently $1).
Contribution memos accept `TOKEN_MINT RECIPIENT_WALLET` or
`RECIPIENT_WALLET TOKEN_MINT`, separated by whitespace (spaces, tabs, or newlines).
A compiled regex requires exactly two Base58 strings, then the Solana SDK
validates both as 32-byte addresses. Exactly one must match a reserved token in
the local catalog; the other is the nonzero receiving wallet. Neither matching,
both matching, extra addresses/text, or invalid addresses leaves the payment
unallocated with a reason. No RPC lookup is needed to identify the token.
The incoming payment amount supplies the dollar contribution; it is not read
from the memo. Existing receipt order, cap, and deadline rules still apply.

Address validation was cross-checked against the installed Solana SDK decoder
(`solana-address`, `FromStr`) and [Solana's account documentation](https://solana.com/docs/core/accounts).
Previously processed receipts are not reinterpreted when this parser changes.

The raise cap
is $12,000 with a 3,600-second window. Late and above-cap amounts remain explicit
unallocated balances. A reserved mint is not yet an on-chain token.

## Cache and storage

The background worker checks the inbox once per second and reads it only when
its file stamp changes. It builds token details, metadata, counts and sorted
indexes together; 256 query responses are retained in each snapshot's LRU.
HTTP reads use memory and ETags, without per-request disk, RPC or Pinata calls.
Projection failures retain the last valid view and mark health unhealthy.

Token responses contain canonical `image_uri` and browser-ready `image_url`.
Uploaded images are served from the existing immutable local cache; the mapping
is restored from saved pins and image files at startup. Other IPFS image URIs
use the configured [gateway](https://docs.pinata.cloud/gateways/retrieving-files).
The token card displays the saved image in its original circular layout.

The private catalog stores reserved mint signing keys, prepared metadata,
processed receipt hashes and contributions. Use filesystem access controls and
private backups; never commit this directory. Public routes explicitly construct
response types rather than serializing private catalog records.

`reprocess_launch <data-dir> <inbox-state> <sequence>` is an offline repair tool
for a previously rejected launch. Stop the catalog owner and back up state
first. It verifies the original receipt hash, exact fee and prepared metadata,
and preserves the existing timestamp and deadline. Repeating a successful
repair returns the same mint.

## Market data

See [market data](market-data.md) for valuation units, persisted curve snapshots,
Jupiter batching, rate limits, and expiration. API list/detail requests only read
the shared projection; they never call Jupiter or Solana. `/api/health.market`
reports request counts and pricing/configuration errors separately from receiver
projection health.
