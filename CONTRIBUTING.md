# Contributing to Jabar

Before opening a pull request:

1. Discuss substantial behavior or architecture changes in an issue.
2. Keep the change focused and explain what changed, why, and how You tested it.
3. Run the relevant checks:

   ```sh
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets --locked -- -D warnings
   cargo test --workspace --locked
   ```

   For changes under `editors/vscode/`, also run `npm ci` and `npm test` there.

4. Identify third-party material and preserve its license and attribution.

## Contributor License Agreement

CLA enforcement is pending the [activation
steps](docs/cla-administration.md). Once enabled, each human contributor must
accept the [Jabar Contributor License Agreement](CLA.md) through the check on
their pull request. Sign once per CLA version. You keep any ownership rights
You have.

If an employer or other organization owns the work, its authorized
representative must also accept for the organization and identify the covered
contributors. Once active, use the private contact published with the CLA to
create or correct an organization record; do not post private employment
records in a pull request.

You are responsible for reviewing generated changes and for having the right
to submit everything in Your contribution.
