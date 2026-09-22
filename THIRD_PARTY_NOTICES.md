# Third-party notices

`frontend-vendor/react.production.min.js` and
`frontend-vendor/react-dom.production.min.js` are React 18.3.1 distributions.
Their existing copyright headers are retained; the upstream MIT license is
included in [frontend-vendor/LICENSE.react](frontend-vendor/LICENSE.react).
Source: [React v18.3.1](https://github.com/facebook/react/tree/v18.3.1).

`frontend/support.js` is the generated DC runtime bundled with the supplied
frontend export. Its original source/provenance header is retained; this
repository does not assign a new license to that supplied runtime.

Rust dependencies are pinned in `Cargo.lock`; the optional browser-test
dependency is pinned in `package-lock.json`. Their respective upstream licenses
apply. Public Pump reference fixtures identify their upstream sources in
[the manifest](docs/verification/manifest.json).
