# Agent guidance for kata-device-plugin

Read [CLAUDE.md](CLAUDE.md) first. This file adds agent-specific constraints on top of it.

## Language and style

- **Rust only.** Never suggest Go, Python, or shell scripts for production code.
- Follow the existing file structure: `main.rs` owns the watcher loop and CLI; `plugin.rs` owns the device plugin gRPC server; `vfio.rs` owns enumeration and PCI classification; `view.rs` owns the fabric view service and its token.
- KISS. Prefer a `match` over a registry, a concrete struct over a trait object, a flat module over a nested one.
- No comments that restate what the code says. Only comment the WHY when it is non-obvious (hardware constraint, security invariant, ADR reference).

## Security invariants — never violate these

- The device plugin must not bind or reconfigure VFIO devices. It reads `/dev/vfio/devices/` to enumerate; allocation returns CDI device names for the Kata shim to resolve. Binding is the trusted side's job.
- No `privileged: true`, no `hostPID`, no `hostNetwork` in the DaemonSet spec.
- No Kubernetes API access — the plugin talks to the kubelet over unix sockets and serves the fabric view on one token-gated TCP port. Do not add a kube client dependency.

## What each ADR means for code changes

| ADR | Code implication |
| --- | --- |
| 1000 | Device plugin only; no DRA driver |
| 10000 | the Helm chart DaemonSet must stay non-privileged |

## Testing

Unit tests live next to the code (`vfio.rs`, `cdi.rs`); integration tests in `tests/server.rs` drive the gRPC server against a mock kubelet on unix sockets. Prefer integration tests that drive the gRPC server over unit tests that mock tonic internals.

## Do not

- Add `serde` or JSON serialisation without a concrete protocol requirement.
- Add a `Registry` struct or `Plugin` trait — the `Mode` enum is the registry.
- Introduce `async-trait` explicitly; use `#[tonic::async_trait]` for tonic impls.
- Add `unwrap()` or `expect()` in async paths — propagate with `?` and `anyhow`.
