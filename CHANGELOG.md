# Changelog

## [1.0.0](https://github.com/hudson-frontier/hudson/compare/v0.1.0...v1.0.0) (2026-10-18)


### ⚠ BREAKING CHANGES

* **harness:** Rust embedders must handle queued input and initialize the new context/profile fields. Strict HTTP clients must accept the reviewed queue lifecycle events. Queue use promotes its PostgreSQL namespace to storage format 2; upgrade all database writers before use.

### Features

* **harness:** add durable input and provider controls ([b790f48](https://github.com/hudson-frontier/hudson/commit/b790f48a553edc32914deb9c83009e0ae2d473fd))
* **harness:** add tool correction and safe provider diagnostics ([#57](https://github.com/hudson-frontier/hudson/issues/57)) ([bdede94](https://github.com/hudson-frontier/hudson/commit/bdede94e9641008feef3e3b4e82cc8de6b40bf86))

## [0.1.0](https://github.com/hudson-frontier/hudson/releases/tag/v0.1.0) (2026-10-17)


### Features

* **api:** qualify a bounded Python preview client ([f86180e](https://github.com/hudson-frontier/hudson/commit/f86180e02ec093cbd07a66a2c6e85578134eeed3))
* **sandbox:** persist fenced requests and qualify owned SDK receipts ([3bd7d47](https://github.com/hudson-frontier/hudson/commit/3bd7d478035420e11e2f1da047583039cd526f39))


### Bug Fixes

* **providers:** retain assistant context across tool continuations ([db59af5](https://github.com/hudson-frontier/hudson/commit/db59af58e2bbacc05dbf263a3ad8e6fb25a69bbc))
