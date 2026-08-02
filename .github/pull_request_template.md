## Summary

Describe the user-visible or engineering outcome and why it is needed.

## Validation

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-features`
- [ ] Relevant Windows checks, property tests, or benchmarks are listed below.

Commands and results:

```text

```

## Safety and completeness

- [ ] Tests only mutate temporary fixtures created by the test.
- [ ] No protected Scanner artifact or copied legacy resource is included.
- [ ] New nontrivial dependencies have a research/license record.
- [ ] User docs, translations, changelog, and traceability are updated when needed.
- [ ] Remaining risks and checks not run are stated explicitly.
