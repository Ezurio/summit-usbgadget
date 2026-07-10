# Agent instructions — summit-usbgadget

## SWUpdate install-result detection (`await_install_result`)

This has been broken repeatedly. Do not "simplify" it without reading this first.

The status frame returned by SWUpdate's `GET_STATUS` (see the daemon's
`core/network_thread.c`) exposes two `RECOVERY_STATUS` fields that look
interchangeable but are NOT:

```c
msg.data.status.current     = instp->status;
msg.data.status.last_result = instp->last_install;
if (a notification is queued)
    msg.data.status.current = notification->status;  // OVERWRITTEN
```

- **`current`** — the installer's live progress phase, overwritten by the
  status of whatever notification is drained from the queue. In practice it only
  reports non-terminal phases (`START` / `RUN` / `DOWNLOAD` / `SUBPROCESS` /
  `PROGRESS`); it does **NOT** carry the terminal verdict, so it must not be used
  to decide success or failure.
- **`last_result`** — `instp->last_install`, the persistent authoritative result
  of the last install: `SUCCESS` or `FAILURE` once terminal, `IDLE` / `PROGRESS`
  while still running.

Both fields decode into the same `RecoveryStatus` enum — that is correct; they
are the same C enum. What differs is the *values you actually observe*:
`current` reports progress phases (and transient `FAILURE`), while `last_result`
holds the terminal verdict.

### Required logic (keep both `blocking.rs` and `async_io.rs` identical)

- **Failure is terminal:** return failure when `last_result` is `FAILURE`.
  (`current` never reports the terminal verdict, so it is not checked.)
- **Success requires confirmation:** return success only when `current == Run`
  **AND** `last_result == Success`. The `Run` gate prevents a stale result from
  a previous install being mistaken for the current one.

```rust
if last_result_now == Some(RecoveryStatus::Failure) {
    return Err(Error::InstallFailed);
}

if current == Some(RecoveryStatus::Run) && last_result_now == Some(RecoveryStatus::Success) {
    return Ok(());
}
```

Any change to this logic must be applied to **both**
`crates/swupdate-ipc/src/blocking.rs` and
`crates/swupdate-ipc/src/async_io.rs`, and the explanatory comment above the
check must stay consistent with the code.
