# Implementation Plan

This document records the implementation plan for `otel-journal-gatewayd-forwarder`. The work described here has already been implemented in the commits preceding this document on `main` (edition 2024, Rust 1.96.1, Range-header cursor transport, TLS/headers, drain/backoff, watchdog/metrics, and the continuation E2E/CI). It is retained as a design record and verification checklist; do not treat it as a fresh set of commits to apply.

## Global constraints

- **No architecture changes.** Keep: blocking reqwest, thread-per-source, module layout (`collector.rs`, `config.rs`, `cursor.rs`, `journal.rs`, `metrics.rs`, `otlp.rs`). No async rewrite, no gRPC/protobuf OTLP, no `?follow` streaming.
- Rust edition 2024, pinned to Rust 1.96.1. Never use openssl-linked crates — musl static builds must keep working.
- After every item: `cargo fmt && cargo test`. Both must be clean before moving on. Once item 2.4 lands, `cargo clippy --all-targets -- -D warnings` joins that gate.
- No new files unless an item says so. No README rewrites beyond sections an item names.
- Conventional commits, one commit per item: `fix: …`, `feat: …`, `test: …`, `docs: …`.

## Verification harness

No local cargo required; run everything in Docker:

```bash
docker run --rm -v "$PWD:/src" -w /src rust:1.96.1-bookworm cargo test
docker run --rm -v "$PWD:/src" -w /src rust:1.96.1-bookworm cargo fmt --check
```

Baseline on the parent commit of this plan: all existing unit tests pass, zero compiler warnings, `cargo clippy --all-targets -- -D warnings` clean, and `cargo audit` clean.

E2E (Phase 1.4) also runs in `rust:1.96.1-bookworm` — it has apt access to `systemd`, `systemd-journal-remote` (which ships both `systemd-journal-remote` and `systemd-journal-gatewayd` on Debian).

---

## Phase 1 — Correctness (production blockers)

### 1.1 `fix`: cursor must go in the `Range` header, not the query string

**File:** `src/journal.rs`, `JournalClient::fetch`.

**Why (verified):** `/entries` accepts only `follow`, `discrete`, `boot`, and `KEY=match` GET params. The current `?cursor=<enc>&skip=1` makes gatewayd return `400 Failed to parse URL arguments` on **every cycle after the first**. Since 400 ≠ 410, the cursor-reset path never fires: each source forwards exactly one batch after a fresh install, then retries the same bad request forever. Cursor syntax per man page: `Range: entries=[cursor][[:num_skip]:[num_entries]]`.

**Change:** replace the body of `fetch` from the start down to (but not including) the `debug!` line with:

```rust
        let mut url = format!("{}/entries", self.base_url);
        let mut query_parts = Vec::new();

        // Cursor goes in the Range header; gatewayd rejects unknown URL params
        // with 400. skip=1 skips the already-forwarded cursor entry.
        let range = if let Some(c) = cursor {
            format!("entries={}:1:{}", c, batch_size)
        } else {
            query_parts.push("boot".to_string());
            format!("entries=:{}", batch_size)
        };

        // Add unit filters
        for unit in &self.units {
            query_parts.push(format!("_SYSTEMD_UNIT={}", urlencoding::encode(unit)));
        }

        if !query_parts.is_empty() {
            url = format!("{}?{}", url, query_parts.join("&"));
        }
```

and change the request builder line

```rust
            .header("Range", format!("entries=:{}", batch_size))
```

to

```rust
            .header("Range", range)
```

**Acceptance:** unit test from 1.3 passes; e2e from 1.4 passes.

### 1.2 `fix`: drop the tail-clamp duplicate entry

**File:** `src/journal.rs`, `JournalClient::fetch`, the `StatusCode::OK` match arm.

**Why (verified):** when the saved cursor is the last journal entry, gatewayd clamps seek+skip at the tail and re-serves the cursor entry itself instead of returning nothing. Measured: one duplicate record per poll per idle source (~17k dup records/day/source at 5s polling).

**Change:** replace the `StatusCode::OK` arm body:

```rust
            StatusCode::OK => {
                // Parse newline-delimited JSON
                let body = response.text()?;
                let mut entries = self.parse_entries(&body)?;
                if let Some(c) = cursor {
                    // gatewayd clamps seek+skip at the journal tail and re-serves
                    // the cursor entry itself; drop it to avoid duplicates.
                    entries.retain(|e| e.cursor != c);
                }
                Ok(entries)
            }
```

**Acceptance:** e2e from 1.4 idle-poll assertion passes (zero duplicates after drain).

### 1.3 `test`: unit tests for request construction

**File:** `src/journal.rs`, extend `mod tests`.

Refactor for testability: extract a pure helper on `JournalClient`:

```rust
    /// Build (url, range_header) for a fetch. Pure; exists for testability.
    fn build_fetch_parts(&self, cursor: Option<&str>, batch_size: usize) -> (String, String)
```

`fetch` must call it. Tests to add:

- no cursor → url ends `?boot`, range == `entries=:500` (batch 500)
- cursor `s=abc;i=1f` → url has **no** `cursor`/`skip`/`boot` params, range == `entries=s=abc;i=1f:1:500`
- with units `["nginx.service"]` + cursor → url query is only `_SYSTEMD_UNIT=nginx.service` (URL-encoded), range still carries the cursor
- dedup: feed `parse_entries` output through the retain logic — an entry whose `__CURSOR` equals the request cursor is dropped; others kept

**Acceptance:** `cargo test` green.

### 1.4 `test`: continuation e2e (the test that would have caught both bugs)

**Files:** new `e2e/continuation-test.sh` + wire into `e2e/run-e2e-test.sh` (or as a second entrypoint in `e2e/run-in-container.sh`). Keep the existing one-shot e2e untouched.

Current e2e runs `--once` in a fresh container, so the cursor branch of `fetch` is **never executed**. This test must exercise it at volume.

Spec (proven approach — copy it):

1. Generate **120 synthetic entries** in journal-export format and import via `systemd-journal-remote --split-mode=none -o /tmp/j/batch1.journal -`. Each entry needs:
   `__REALTIME_TIMESTAMP` (current µs), `__MONOTONIC_TIMESTAMP` (**strictly increasing, e.g. `1000000 + i*100000`**), `_BOOT_ID` (from `/proc/sys/kernel/random/boot_id`, dashes stripped), `_HOSTNAME`, `_SYSTEMD_UNIT=test.service`, `PRIORITY=6`, `MESSAGE=test message %04d`.
   **Gotcha (cost a debugging round):** omitting `__MONOTONIC_TIMESTAMP` gives every entry `m=0`, which breaks journal interleaving across files and produces false cursor failures. Real journals never have this; synthetic ones must set it.
2. Serve with `systemd-socket-activate -l 19531 /lib/systemd/systemd-journal-gatewayd -D /tmp/j` (no PID-1 systemd needed).
3. OTLP sink: trivial HTTP server writing each POST body to `/tmp/sink/reqNNN.json`, responding `200 {}`.
4. Forwarder config: `batch_size = 10`, `poll_interval = "1s"`, `cursor_dir = /tmp/cur` → **12 chained cursor cycles required**.
5. Run continuous for ~40s (`timeout --signal=TERM 40 … || true`).
6. Append 120 more entries (121–240, monotonic continuing upward) as `batch2.journal`; **restart gatewayd**; run a **fresh forwarder process** for ~40s.
7. Verify across ALL sink files cumulatively: extract trailing message numbers; assert `records == unique == 240`, zero duplicates, zero missing, non-decreasing order. Exit non-zero on failure.

Expected: phase 1 = exactly 12 push cycles then silent idle polls; phase 2 = exactly 12 more; cursor file changes seqnum-id (`s=`) when crossing into batch2's file.

**Acceptance:** script passes locally in `rust:1.96.1-bookworm`; fails when run against unpatched `main` (spot-check once to prove the test works).

### 1.5 `chore`: compiler warnings

The compiler warnings that existed on `origin/main` have already been resolved in the preceding implementation commits. Verify with `cargo build` that zero warnings remain. **Acceptance:** `cargo build` emits zero warnings.

---

## Phase 2 — Toolchain, dependencies & supply chain

Execute immediately after Phase 1. From 2.4 onward, every subsequent item in every phase must also keep `cargo clippy --all-targets -- -D warnings` and `cargo audit` green.

### 2.1 `chore`: pin toolchain + MSRV

**Files:** new `rust-toolchain.toml`, `Cargo.toml`, `.gitlab-ci.yml`, `PLAN.md` harness section.

Pin to the current stable at implementation time (check with `docker run --rm rust:1.96.1-bookworm rustc --version`):

```toml
# rust-toolchain.toml
[toolchain]
channel = "<current stable, e.g. 1.XX.0>"
components = ["rustfmt", "clippy"]
```

Set `rust-version = "<same>"` under `[package]` in `Cargo.toml`. Pin the CI/docker image to the matching `rust:<version>-bookworm` everywhere `rust:1.96.1-bookworm` appears. **Acceptance:** `cargo build` green on the pinned image.

### 2.2 `chore`: migrate to edition 2024

**File:** `Cargo.toml` + whatever `cargo fix` touches.

```bash
cargo fix --edition --allow-dirty && \
  sed -i 's/edition = "2021"/edition = "2024"/' Cargo.toml && \
  cargo fmt && cargo test
```

Review the diff — expect mechanical changes only (if-let temporaries, unsafe attrs). Do not hand-edit beyond what `cargo fix` produces. **Acceptance:** zero warnings, all tests green.

### 2.3 `chore`: refresh lockfile

`cargo update`, then `cargo test`. All deps are already on current majors (reqwest 0.12, clap 4, thiserror 2, toml 0.9, humantime 2) — expect patch/minor bumps only. If any **major** bump is available, note it in the commit message but do NOT migrate it in this item. **Acceptance:** tests green.

### 2.4 `ci`: clippy gate

Fix all `cargo clippy --all-targets -- -D warnings` findings (expect a handful; keep fixes minimal and mechanical), then add the command as a CI job in `.gitlab-ci.yml`. **Acceptance:** clippy clean locally and in CI config.

### 2.5 `ci`: cargo-audit gate

Add a CI job: `cargo install cargo-audit --locked && cargo audit` (blocking). If an advisory has no fix available, add it to `audit.toml` ignore list with a comment linking the RUSTSEC id. Rationale: this project's value proposition is a small auditable surface on hypervisors — CVE hygiene is part of the contract. **Acceptance:** job passes.

### 2.6 `refactor`: replace raw libc signal handler with signal-hook

**Files:** `Cargo.toml`, `src/main.rs`.

Current: `libc::signal` with an `unsafe` extern handler writing a `OnceLock<Arc<AtomicBool>>`. Replace with:

```toml
[target.'cfg(unix)'.dependencies]
signal-hook = "0.3"   # replaces libc
```

```rust
#[cfg(unix)]
{
    signal_hook::flag::register(signal_hook::consts::SIGTERM, shutdown.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, shutdown.clone())?;
}
```

Delete `SHUTDOWN_FLAG`, `handle_signal`, the `libc` dependency, and all `unsafe`. **Acceptance:** e2e continuation test still terminates cleanly on `timeout --signal=TERM` (graceful shutdown log line present); zero `unsafe` blocks remain in the crate.

### 2.7 `chore`: fix package metadata

**File:** `Cargo.toml`: `authors = ["Your Name <you@example.com>"]` is a placeholder — set the real author. Bump `version` to `0.2.0` at the end of Phase 2 (Phase 1 + 2 together are the first correct release). **Acceptance:** `cargo package --list` runs without warnings about metadata.

---

## Phase 3 — Transport security (mTLS + auth)

### 3.1 `feat`: rustls + gzip in reqwest

**File:** `Cargo.toml`. Replace the reqwest line:

```toml
reqwest = { version = "0.12", default-features = false, features = ["blocking", "json", "rustls-tls", "gzip"] }
```

`gzip` makes reqwest send `Accept-Encoding: gzip` and transparently decompress — journal JSON is highly redundant; this is the WAN leg win. **Acceptance:** `cargo test` green; `cargo build --target x86_64-unknown-linux-musl` still links (run in docker; add `rustup target add` first).

### 3.2 `feat`: TLS + header config

**File:** `src/config.rs`.

New TOML surface (global `[tls]` is the default; a source-level `tls` table overrides whole-sale; all fields optional):

```toml
otlp_headers = { Authorization = "Bearer …" }  # global, OTLP leg

[tls]
ca_cert = "/etc/ojgf/ca.pem"          # extra root CA (PEM)
client_cert = "/etc/ojgf/client.pem"  # mTLS client cert (PEM)
client_key = "/etc/ojgf/client.key"   # mTLS client key (PEM)

[[sources]]
name = "public-gateway"
url = "https://gw.example:19531"
headers = { Authorization = "Basic …" }        # per-source, gatewayd leg
tls = { ca_cert = "/etc/ojgf/other-ca.pem" }
```

Add `TlsConfig { ca_cert, client_cert, client_key: Option<PathBuf> }` (Deserialize + Clone), `tls: Option<TlsConfig>` on both `Config` and `Source`, `headers: HashMap<String, String>` (default empty) on `Source`, `otlp_headers` on `Config`. Validation: if exactly one of `client_cert`/`client_key` is set → `ConfigError::InvalidValue`. Resolution: source `tls` if set, else global.

### 3.3 `feat`: apply TLS + headers to both HTTP clients

**Files:** `src/journal.rs` (`JournalClient::new`), `src/otlp.rs` (`OtlpClient::new`), call sites in `src/collector.rs` / `src/main.rs`.

Shared builder helper (put it in `src/config.rs` or a small `src/http.rs`):

```rust
pub fn build_client(tls: Option<&TlsConfig>, headers: &HashMap<String, String>, timeout: Duration)
    -> Result<reqwest::blocking::Client, …>
```

- `ca_cert` → `Certificate::from_pem` + `.add_root_certificate(…)`
- `client_cert`+`client_key` → concatenate the two PEM files, `Identity::from_pem(…)` + `.identity(…)` (rustls identity takes cert-then-key in one PEM buffer)
- headers → `default_headers`
- Fail fast at startup with a clear error naming the bad file.

**Acceptance:** unit tests for config parse/validation; manual smoke doc in README (`--validate` prints whether TLS is active per source). E2E stretch (optional, separate commit): self-signed CA via openssl, gatewayd with `--cert=/--key=/--trust=`, forwarder with matching `[tls]` — assert entries flow and that a client **without** a cert is rejected.

### 3.4 `docs`: README security section

Document: gatewayd serves full journal contents unauthenticated by default; firewall it or use `--cert=`/`--key=` (HTTPS, systemd ≥198) + `--trust=` (client-cert verification, systemd ≥236); forwarder `[tls]`/`headers` config; reverse-proxy (Caddy) fronting as the no-code alternative.

---

## Phase 4 — Robustness

### 4.1 `feat`: drain loop (catch-up after downtime)

**File:** `src/collector.rs`.

`poll()` currently fetches one batch per cycle → max throughput `batch_size/poll_interval` (default 100/s). A day of backlog on a chatty host takes hours.

Change `run_loop` cycle to: call `poll()` repeatedly **within one cycle** while it returns `Ok(n) where n == batch_size` (i.e. probably more pending), up to a safety cap `max_drain_batches = 100` per cycle, checking `shutdown` between iterations. Sleep only after draining. `--once` mode: repeat drain cycles (without sleeping) until a batch is short, i.e. the source is caught up, then exit. Do not let the per-cycle cap leave data behind in once mode.

**Acceptance:** unit-level: not practical — cover via e2e: with 240 pending entries and `poll_interval = "30s"`, a single cycle drains everything (watch sink request count within ~5s). Add a once-mode case with >100 batches pending and verify the source drains fully before exiting.

### 4.2 `feat`: exponential backoff on consecutive failures

**File:** `src/collector.rs`, `run_loop`.

Track `consecutive_failures: u32`; on `Err` sleep `min(poll_interval * 2^n, 300s)` instead of `poll_interval`; reset on success. Keep the existing 100ms shutdown-check granularity. **Acceptance:** unit test the delay computation (extract `fn backoff_delay(base: Duration, failures: u32) -> Duration`).

### 4.3 `feat`: cap extra-field size

**Files:** `src/config.rs` (`max_field_bytes: usize`, default `8192`, env `OJGF_MAX_FIELD_BYTES`), `src/journal.rs` (`From<RawJournalEntry>`: truncate each `extra_fields` value to the cap on a char boundary, append `…[truncated]`), thread the value through `JournalClient`. Journal entries can carry multi-hundred-KB fields (coredumps, audit); unbounded copies inflate OTLP payloads. **Acceptance:** unit test: oversized field truncated, `MESSAGE` unaffected.

### 4.4 `docs`: document the 410 duplicate window

**File:** README (cursor-management section). On 410 the forwarder resets to current boot → the current boot is re-ingested → duplicates in the backend. State it plainly; recommend generous journald retention on sources. No code change.

---

## Phase 5 — Observability & ops

### 5.1 `feat`: lag + last-success metrics

**Files:** `src/metrics.rs`, `src/collector.rs`.

Add to `SourceMetrics`: `last_entry_realtime_us: Option<u64>`, `last_success_timestamp: Option<f64>`. Record both in `collector.poll()` after a successful OTLP push (`last_entry_realtime_us` = last forwarded entry's `realtime_timestamp`). Render (follow the existing naming prefix in `render()`):

- `…_source_lag_seconds{source=…}` = `now - last_entry_realtime_us/1e6`
- `…_last_success_timestamp_seconds{source=…}`

These are the alerting signals ("host X logs stale for 15m"). **Acceptance:** extend `test_metrics_render`.

### 5.2 `feat`: `/healthz` on the metrics server

**File:** `src/metrics.rs`, `handle_request`: `GET /healthz` → `200 ok`. Everything else unchanged. **Acceptance:** unit test if the handler is testable, else e2e curl.

### 5.3 `feat`: sd_notify readiness + watchdog

**Files:** `Cargo.toml` (`sd-notify = "0.4"`, unix-only dep), `src/main.rs`, `src/collector.rs`.

- After all collector threads spawn: `sd_notify::notify(false, &[NotifyState::Ready])`.
- Each collector updates a shared per-source `AtomicU64` (epoch seconds) at the end of every cycle (success **or** handled failure — liveness, not health).
- Main thread: instead of bare `join`, loop every `WATCHDOG_USEC/2` (via `sd_notify::watchdog_enabled`): if every source ticked within `max(5 * poll_interval, 60s)`, send `Watchdog`. A wedged thread ⇒ no ping ⇒ systemd restarts the unit.
- No-op when not under systemd (env vars absent).

**Acceptance:** builds on non-systemd (macOS docker) fine; manual verification note in README.

### 5.4 `docs`: hardened unit file in README

Update the README systemd unit: `Type=notify`, `WatchdogSec=90`, `DynamicUser=yes`, `StateDirectory=otel-journal-gatewayd-forwarder` (drop the manual `mkdir /var/lib/…` step). Code change in `src/config.rs`: cursor-dir resolution order becomes `OJGF_CURSOR_DIR` > config file > `$STATE_DIRECTORY` (set by systemd) > compiled default. **Acceptance:** unit test for the resolution order (set/unset `STATE_DIRECTORY`).

### 5.5 `feat`: journald-convention attributes on log records

**File:** `src/otlp.rs`, `build_log_record`.

Also emit (alongside existing attributes): `journald.unit.name` (= `_SYSTEMD_UNIT`, when present) and `journald.priority.number` (= raw priority as int attr). Rationale: otelcol-contrib journald pipelines commonly use these names; emitting both keeps Loki queries uniform across push-based (otelcol) and pull-based (this) hosts. **Acceptance:** extend `test_build_payload`.

---

## Phase 6 — CI & tooling

### 6.1 `feat`: docker fallback in e2e runner

**File:** `e2e/run-in-container.sh`: pick `podman` if present, else `docker` (`CONTAINER_RT=${CONTAINER_RT:-$(command -v podman || command -v docker)}`). **Acceptance:** runs on a docker-only host.

### 6.2 `ci`: run the continuation e2e in CI

**File:** `.gitlab-ci.yml`: add a job running `e2e/continuation-test.sh` (pinned toolchain image from item 2.1, no container-in-container needed since the test runs flat). Keep existing jobs untouched.

---

## Non-goals (do not implement)

- async/tokio rewrite; OTLP gRPC or protobuf encoding; `?follow` / SSE streaming; per-source poll-interval overrides; config hot-reload; Windows support.

## Definition of done (whole plan)

- `cargo fmt --check`, `cargo build` (zero warnings), `cargo clippy --all-targets -- -D warnings`, `cargo audit`, `cargo test` — all green on the pinned toolchain image.
- Edition 2024, pinned toolchain, MSRV set, zero `unsafe`.
- `e2e/continuation-test.sh` green.
- musl release build succeeds.
- README updated only where items say so.
