# Deployment

The Rust backend serves the website and API together, or you can host the
static site separately while the API stays on your own server. Keep credentials
and runtime paths in private environment files. Copy `.env.example` and configure
your own addresses and credentials. Service templates are under `tools/`.

## Serve the site and API together

```sh
cargo build --release --locked --bins
target/release/launchpad "$WEB_BIND_ADDRESS" \
  "$INDEXER_DATA_DIR/state.json" "$LAUNCHPAD_DATA_DIR" \
  frontend frontend-api frontend-vendor
```

Start the notification receiver first so its state file exists. The backend
requires `PINATA_JWT` and `IPFS_GATEWAY`. Back up durable state outside Git.

## Export a static site

Pages are assembled from `frontend/` and `frontend-api/` with the same page table
and adapters as the live server. The checked-in `public/` uses same-origin API
URLs and contains no deployment-specific hostname. To host it separately,
regenerate the pages with your own API origin:

```sh
cargo build --locked --bin export_static_site
target/debug/export_static_site "$API_ORIGIN" public \
  frontend frontend-api frontend-vendor
```

`API_ORIGIN` must be a bare HTTPS origin (HTTP on loopback is accepted for local
tests). Pass an empty string for same-origin behavior. The exporter embeds the
origin in the output, so regenerate the pages whenever it changes.

`render.yaml` is a static-site template publishing `public/`. Export the site
before deployment; Render does not compile the Rust application.

## Cross-origin requests and tunnels

For a separately hosted frontend, set `SITE_ORIGIN` to its exact HTTPS origin.
The API grants browser access only to that configured origin. The shared
`window.xlaunchApi` helper resolves API and media URLs for every page adapter.

A Cloudflare tunnel is optional. `tools/cloudflared-tunnel-config.yml` is a
placeholder template: supply your own tunnel identifier, hostname, private
credentials path and API port before use. Configure the tunnel service through
its private environment file. No tunnel credential, session, hostname or
server-specific port is included here. Consult Cloudflare's current official
instructions when provisioning the tunnel.

## Checks

Verify that `/healthz` and `/api/config` respond, the configured frontend origin
receives the expected CORS headers, and an unrelated origin does not. Check that
all exported pages fetch data and images from your configured API origin.
