# Team observability — fleet metrics in Grafana

claudectl is a local tool, but the supervisor already tracks everything a team
lead wants to see across a fleet of agents: how many tasks are running, how much
they've spent, how often they retry, and whether verifiers are passing. This
page turns that into a dashboard.

The bridge is a Prometheus exporter built into the binary. No agent, no
sidecar, no claudectl-specific glue in your monitoring stack — just a
`/metrics` endpoint that Grafana, Datadog, or any Prometheus-compatible system
scrapes.

```
 ┌────────────┐  reconciles   ┌──────────────┐  scrape /metrics  ┌──────────┐
 │ supervisor │──────────────▶│  coord.db    │◀──────────────────│Prometheus│
 │  (tick)    │   writes      │  (SQLite/WAL)│   claudectl        └────┬─────┘
 └────────────┘               └──────────────┘   supervisor metrics    │
                                                                    ┌───▼────┐
                                                                    │Grafana │
                                                                    └────────┘
```

## 1. Run the exporter

```bash
claudectl supervisor metrics 0.0.0.0:9464
```

- Binds `127.0.0.1:9464` by default; pass `0.0.0.0:9464` to expose it on the LAN
  for a shared Prometheus.
- Each scrape opens the coord DB fresh (WAL), so this is safe to run **alongside**
  the reconciler — they don't contend for the connection.
- Blocks until Ctrl-C. For a long-lived deployment, run it under systemd (see
  [Running as a service](#running-as-a-service)).

Verify it directly:

```bash
curl -s http://localhost:9464/metrics
```

## 2. Point Prometheus at it

Add a scrape job to `prometheus.yml`:

```yaml
scrape_configs:
  - job_name: claudectl
    scrape_interval: 15s
    static_configs:
      - targets: ["localhost:9464"]   # or the host running the exporter
        labels:
          fleet: primary
```

The `fleet` label is optional but recommended — it lets one Prometheus/Grafana
serve several teams or machines. The bundled dashboard is fleet-agnostic and
aggregates across whatever it scrapes.

## 3. Import the Grafana dashboard

The repo ships a ready dashboard at
[`docs/grafana/claudectl-fleet.json`](grafana/claudectl-fleet.json).

1. Grafana → **Dashboards → New → Import**.
2. Upload `claudectl-fleet.json` (or paste its contents).
3. Pick your Prometheus data source when prompted.

You get five panels: tasks by state, fleet cost, retries by cause, verifier
pass rate, and a running/blocked headline.

## What the metrics mean

| Metric | Type | Labels | Reading |
|---|---|---|---|
| `claudectl_tasks_by_state` | gauge | `state` | How many tasks sit in each lifecycle state (`RUNNING`, `PENDING`, `VERIFYING`, `DONE`, `FAILED`, …). Watch `FAILED` and a growing `PENDING` backlog. |
| `claudectl_fleet_cost_usd_total` | counter | — | Cumulative USD across every attempt and verifier. Graph `increase(...[1h])` for burn rate. |
| `claudectl_retries_total` | counter | `cause` | Transitions into `RETRYING`/`RESUMING`, bucketed by cause (`verify_fail`, `timeout`, …). A rising `verify_fail` rate means work is landing but not passing checks. |
| `claudectl_verifier_pass_rate` | gauge | `kind` | Pass fraction (0–1) per verifier kind (`run`, `brain`, `agent`). A dip is the earliest signal that a class of tasks has started regressing. |

## Alerting starters

Two rules cover most of what a lead cares about:

```yaml
groups:
  - name: claudectl
    rules:
      - alert: ClaudectlBurnRateHigh
        expr: increase(claudectl_fleet_cost_usd_total[1h]) > 20
        for: 10m
        annotations:
          summary: "Fleet spend > $20/hr"

      - alert: ClaudectlVerifierRegressed
        expr: claudectl_verifier_pass_rate < 0.6
        for: 15m
        annotations:
          summary: "Verifier {{ $labels.kind }} passing < 60%"
```

Tune the thresholds to your fleet — start loose, tighten once you know your
baseline.

## Running as a service

A minimal systemd unit so the exporter survives reboots:

```ini
# /etc/systemd/system/claudectl-metrics.service
[Unit]
Description=claudectl fleet metrics exporter
After=network.target

[Service]
ExecStart=/usr/local/bin/claudectl supervisor metrics 0.0.0.0:9464
Restart=on-failure
User=%i

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl enable --now claudectl-metrics
```

## Notes

- The exporter only reads. It never mutates coord state, so running it can't
  affect task execution.
- `/metrics` is the only route; everything else returns 404. There's no auth —
  bind to `127.0.0.1` and scrape locally, or put it behind your existing
  network controls before exposing it.
- Metrics come from the coord DB, so they reflect supervisor-managed tasks. Ad
  hoc sessions that never became supervisor tasks won't appear here.
