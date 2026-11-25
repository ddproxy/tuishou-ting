# Heartbeat health fields

The `health` module supplies three additive heartbeat fields so a controller
can stop routing jobs to a node whose harness endpoint is dead or whose
workspace filesystem is full. The module does not send heartbeats or open a
listener: the agent attaches the rendered fields to the heartbeat it already
sends.

## Wire fields

| Field | Type | Meaning when present |
| --- | --- | --- |
| `harness_ok` | bool | `true` when the local harness HTTP endpoint produced an HTTP reply at the last probe; `false` when it did not. |
| `disk_free_pct` | integer 0-100 | Percentage of the workspace filesystem available to unprivileged users. |
| `disk_free_bytes` | integer | Workspace filesystem bytes available to unprivileged users. |

Every field is optional. `HeartbeatHealth::to_json()` emits only the fields
that were sampled, so an unavailable signal is omitted rather than sent as a
wrong value. An entirely empty payload is `{}`.

```json
{"harness_ok":true,"disk_free_pct":25,"disk_free_bytes":250}
```

These keys are additive: no existing heartbeat key is renamed, retyped or
removed. A controller that ignores unknown fields can keep operating with the
old payload shape; a controller that understands the keys can avoid a sick
node. No compatibility document is mirrored in this repository, so no local
compatibility rule was available to apply.

## Sampling

`HeartbeatSampler::health()` returns a sample for the current heartbeat and
reuses it until `cache_interval` has elapsed (default 5 seconds). On a cache
miss it takes one harness probe and one filesystem read, publishes the result
to the shared `NodeMetrics`, and caches it. Sampling happens outside the cache
lock so a slow probe cannot hold up another caller.

### Harness probe

`TcpHarnessProbe` targets a caller-supplied local `SocketAddr`. It sends a
plain `GET / HTTP/1.1` request with no credentials, and counts any reply
starting with `HTTP/` as an answer, including an error status: the signal is
liveness of the harness, not the health of one route.

`HealthConfig::probe_timeout` (default 200 ms) is one shared deadline for
connect, request and reply. Each stage gets only the time left in that budget,
so a heartbeat never waits longer than the probe timeout for a dead harness.
Connection failures, write failures, short or malformed replies and timeouts
all produce `harness_ok: false`; none is an error or a panic.

### Workspace disk

`SystemStatvfs` calls `statvfs` once for the workspace path. `disk_free_bytes`
uses `f_bavail * f_frsize` (space available to unprivileged users, so reserved
blocks are not counted as free). `disk_free_pct` is that quotient over
`f_blocks * f_frsize`, clamped to 0-100. A failed or overflowing read omits
both disk fields. The percentage deliberately includes reserved blocks in the
total, matching what statvfs reports for the filesystem.

## Wiring the agent

```rust
use std::sync::Arc;
use tuishou_ting::health::{
    HealthConfig, HeartbeatSampler, SystemStatvfs, TcpHarnessProbe,
};
use tuishou_ting::orphan::NodeMetrics;

let metrics = Arc::new(NodeMetrics::default());
let sampler = HeartbeatSampler::new(
    HealthConfig::new(),
    "/srv/workspace",
    Box::new(TcpHarnessProbe::new("127.0.0.1:4096".parse().unwrap())),
    Box::new(SystemStatvfs),
    Arc::clone(&metrics),
);
let fields = sampler.health().to_json();
```

The agent supplies the harness listener address and the workspace path; this
repository contains neither. Tests inject `HarnessProbe` and `Statvfs`
implementations, so no live server or real filesystem state is needed. The
clock is also replaceable with `with_clock` for deterministic cache tests.

## Metrics

`NodeMetrics::render()` already produces the text served by the node metrics
endpoint. When a health sample exists it also emits:

```text
# HELP harness_ok Whether the harness endpoint answered the last probe.
# TYPE harness_ok gauge
harness_ok 1
# HELP disk_free_pct Workspace filesystem space available to unprivileged users.
# TYPE disk_free_pct gauge
disk_free_pct 25
# HELP disk_free_bytes Workspace filesystem bytes available to unprivileged users.
# TYPE disk_free_bytes gauge
disk_free_bytes 250
```

`harness_ok` is `1` for answered and `0` for unanswered. Gauges for fields
with no sample are omitted rather than exported as zero.
