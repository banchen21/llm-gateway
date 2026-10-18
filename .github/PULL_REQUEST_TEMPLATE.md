## What this changes

<!-- One paragraph. If it fixes an issue, link it. -->

## Why

<!-- The behaviour that prompted it. -->

## Checks

- [ ] `cargo fmt --check`
- [ ] `cargo clippy -p gateway-core -p gateway-server --all-targets -- -D warnings`
- [ ] `cargo test -p gateway-core -p gateway-server`
- [ ] `cd apps/desktop && pnpm build`
- [ ] New behaviour is covered by a test, or the PR says why it cannot be

## Notes for the reviewer

<!-- Protocol changes: say which of the four inbound/upstream combinations you exercised. -->
