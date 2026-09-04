# Running the collector

`tok collector` is the background writer: it polls every configured account on the local cadence
(`[settings].poll_local_secs`) and writes snapshots + derived limits to the SQLite store. The TUI
(and `tok collector --once`) read that store. It is ToS-safe (local reads only — a native transcript
scan by default, `ccusage` if you opt into that lane) until you opt an account into the overlay.

- **One-shot** (cron-friendly): `tok collector --once` — collect once, persist, print a read-back
  summary, exit.
- **Daemon** (24/7): `tok collector` — loop until `SIGINT`/`SIGTERM`, writing a `heartbeat` each tick.

## Picking `poll_local_secs`

Claude usage is read by the native in-process transcript scanner (spec 026) by default: the first
tick reads every file touched in the last 24h, and later ticks re-read only files that changed since
the last tick (per-file incremental cache), with N accounts whose config dirs share one transcripts
tree (symlinked `projects/`) sharing a single scan per tick. Cost is bounded by recent activity, not
history size. Default is 60s; raise to ~300s if you've opted into the `ccusage` subprocess lane
(`ccusage_cmd`), which re-reads all transcripts every tick and scales with history size — on one
real 7-account machine under that lane a 20s cadence held load at ~44 on 16 threads.

## systemd `--user` unit (optional; not auto-installed)

Run the daemon as a user service so it starts with your session and restarts on failure. Adjust the
`ExecStart` path to wherever you installed the `tok` binary (e.g. `~/.local/bin/tok` or the built
`target/release/tok`).

```ini
# ~/.config/systemd/user/tokenomics-collector.service
[Unit]
Description=Tokenomics collector (LLM subscription usage → local store)
After=default.target

[Service]
Type=simple
ExecStart=%h/.local/bin/tok collector
Restart=on-failure
RestartSec=10
# tok reads ~/.config/tokenomics/tokenomics.toml and writes the XDG data-dir store.
# Contain any regression to this unit instead of letting systemd-oomd pick bystanders in the
# user slice (spec 026 NFR-2).
MemoryHigh=768M
MemoryMax=1G

[Install]
WantedBy=default.target
```

Install and start:

```bash
systemctl --user daemon-reload
systemctl --user enable --now tokenomics-collector.service
systemctl --user status tokenomics-collector.service
journalctl --user -u tokenomics-collector.service -f
```

To keep it running after you log out (headless WSL/servers):

```bash
loginctl enable-linger "$USER"
```

Stop / disable:

```bash
systemctl --user disable --now tokenomics-collector.service
```
