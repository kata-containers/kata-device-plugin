# Agent guidance for kata-device-plugin

Read [CLAUDE.md](CLAUDE.md) first. This file adds agent-specific constraints on top of it.

## Language and style

- **Rust only.** Never suggest Go, Python, or shell scripts for production code.
- Follow the existing file structure: `main.rs` owns the dispatch loop and CLI; `plugin.rs` owns the gRPC server and `Mode` enum; `labels.rs` is constants only.
- KISS. Prefer a `match` over a registry, a concrete struct over a trait object, a flat module over a nested one.
- No comments that restate what the code says. Only comment the WHY when it is non-obvious (hardware constraint, security invariant, ADR reference).

## Security invariants — never violate these

- The device plugin must not bind or reconfigure VFIO devices. It reads `/dev/vfio/` to enumerate; allocation returns device paths for the kubelet to inject. Binding is the trusted side's job.
- `Mode::Imex` must not advertise devices until `kata.nvidia.com/nvlink-clique-id` is present. The matcher in `select_mode()` enforces this; do not weaken it.
- No `privileged: true`, no `hostPID`, no `hostNetwork` in the DaemonSet spec.

## What each ADR means for code changes

| ADR | Code implication |
|---|---|
| 1000 | Device plugin only; no DRA driver |
| 4000 | Host-supplied values (clique-id label) are advisory — a wrong value fails the job, never a security break |
| 8000/9000 | `select_mode()` gates `Imex` on the clique label being non-empty |
| 10000 | `deploy/daemonset.yaml` must stay non-privileged |

## When adding a new `Mode` variant

1. Add the label constant to `src/labels.rs`.
2. Add the variant to `enum Mode` in `src/plugin.rs` (derive `Clone + PartialEq`).
3. Add the match arm to `select_mode()` in `src/main.rs`.
4. Add the runtime behaviour inside `DeviceServer::run()` in `src/plugin.rs` — only inside the mode branch, no other changes.
5. Update `CLAUDE.md` label table.

## Testing

No test suite exists yet. When adding tests, prefer integration tests that drive the gRPC server over unit tests that mock tonic internals.

## Do not

- Add `serde` or JSON serialisation without a concrete protocol requirement.
- Add a `Registry` struct or `Plugin` trait — the `Mode` enum is the registry.
- Introduce `async-trait` explicitly; use `#[tonic::async_trait]` for tonic impls.
- Add `unwrap()` or `expect()` in async paths — propagate with `?` and `anyhow`.
