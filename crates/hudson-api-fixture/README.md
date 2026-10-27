# Hudson API test fixture

This unpublished workspace crate supplies `hudson-api-fixture` for backend
conformance tests. It is not the Hudson operator CLI. Install the supported
[Hudson CLI](https://github.com/hudson-frontier/hudson-cli#installation) and use
`hudson agent` to operate a running service.

The fixture retains bounded HTTP response/file handling, authentication checks,
event cursors, and terminal regressions used by this repository's smoke tests.
It installs no `hudson` or `hudson-cli` binary, so backend development cannot
replace the standalone client. Add public commands in the standalone repository;
keep this fixture limited to backend protocol coverage.

Run the normal checks from the repository root. For an explicit compatibility
check with an external CLI, see
[HTTP integration](../../docs/development.md#http-integration).
