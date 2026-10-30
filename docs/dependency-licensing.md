# Runtime dependency licensing

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
