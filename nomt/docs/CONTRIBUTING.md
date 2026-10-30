## Contribute to NOMT

We license all code under MIT / Apache2.0 licenses. The maintainers reserve the right to refuse contributions and reject issues, even when useful.

## Formatting

We use spaces for indentation and adhere to the vanilla `rustfmt` style.

Format your code using `rustfmt`:
  1. `cargo install cargo-fmt`
  2. `cargo fmt --all`

## Documentation Policy

Well-commented code is readable code. We require all `pub` and `pub(crate)` items to be annotated with doc-strings. This leads to much better auto-generated documentation pages using `rustdoc` and a better experience for library users.

Public modules and crates should begin with doc-strings which explain the purpose of the module and crate and assist the reader in determining where to proceed.

## Pull Requests and Tests

We require that the entire test-suite passes for every merged PR. A PR is the responsibility of the author. In submitting a PR, you are consenting to become responsible for it and continually improve, update, and request reviews for it until merged. Stale PRs are not the responsibility of the maintainers and may be closed.

## Code of Conduct

We ask that all contributors maintain a respectful attitude towards each other.