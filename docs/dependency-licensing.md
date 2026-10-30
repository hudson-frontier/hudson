# Runtime dependency licensing

The optional Python agent peer uses OpenAI Agents SDK 0.24.0 (MIT), OpenAI
Python SDK 3.41.0 (Apache-2.0), and httpx2 2.16.0 (BSD-3-Clause). They are
installed as dependencies, with their original package license metadata, rather
than vendored implementations. The peer itself remains Apache-2.0.

Hudson remains Apache-2.0. Cargo.lock records the resolved dependency versions;
distributions must retain the licenses and notices required by those packages.
Inline image transport uses `base64` 0.22 under MIT OR Apache-2.0. It decodes
bounded transport bytes without introducing an image codec or executing files.
No external runtime implementation is vendored into the execution bridge.

The Windows process lifetime adapter uses `process-wrap` 10.0.1 under the
operator's choice of Apache-2.0 or MIT. It uses the MIT-licensed `windows` 0.62
bindings for process creation flags. Both are target-specific dependencies.
`process-wrap` assigns a suspended child to a Job Object before resuming it and
enables kill-on-close cleanup. Its source remains in the Cargo dependency rather
than introducing unsafe Windows calls into Hudson.

The event transport enables Axum's WebSocket support and uses `futures` 0.3
(MIT OR Apache-2.0). Axum's `tokio-tungstenite`/`tungstenite` 0.29 transport and
the matching test client are MIT-licensed. These licenses are compatible with
Hudson's distribution policy. Third-party components retain their own copyright
and license metadata.
## Native CLI integration

The optional operator-installed Codex CLI 0.165.0 is Apache-2.0. Hudson does not
redistribute its binaries or source. The native peer and gateway use Python's
standard library and first-party private checkpoint helpers; the SDK packages
are not required for this peer. Preserve the installed CLI package's license and
third-party notices.
