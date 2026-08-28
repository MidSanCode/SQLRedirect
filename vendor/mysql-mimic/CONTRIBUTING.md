# Contributing

Thanks for your interest in contributing.

## Development Setup
1. Install Rust stable toolchain.
2. Clone the repository.
3. Run:

```bash
cargo build
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

## Pull Request Guidelines
- Keep changes focused and small.
- Add tests for new behavior and bug fixes.
- Update docs for user-visible changes.
- Ensure CI is green before requesting review.

## Commit and Branching
- Use descriptive commit messages.
- Rebase/squash your branch as needed before merge.

## Security Issues
Please do not open public issues for security vulnerabilities. See SECURITY.md.
