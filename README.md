# Fauna Scan

Fauna Scan is a Linux Rust service that discovers Hikvision cameras, searches
NVR picture metadata, downloads new JPEGs into SQLite-backed durable state, and
classifies them through one or more OpenAI-compatible vision API endpoints.

## Install and release verification

Build with the current stable Rust toolchain:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --no-fail-fast
cargo build
cargo build --release
install -Dm755 target/release/fauna-scan ~/.local/bin/fauna-scan
```

Copy `config.example.toml` to `${XDG_CONFIG_HOME:-$HOME/.config}/fauna-scan/config.toml`.
Persistent state defaults to `${XDG_STATE_HOME:-$HOME/.local/state}/fauna-scan/`;
cache is conventionally `${XDG_CACHE_HOME:-$HOME/.cache}/fauna-scan/`. The image
output directory is explicit and may be outside state.

Replace every `/home/user` placeholder with the real home and NVR/classifier
values. Create empty secret files securely, then populate them with a secure editor (do
not place the values in shell history):

```bash
mkdir -p ~/.config/fauna-scan ~/.local/state/fauna-scan ~/Pictures/fauna-scan
install -m 600 /dev/null ~/.config/fauna-scan/nvr-password
install -m 600 /dev/null ~/.config/fauna-scan/classifier-api-key
${EDITOR:-vi} ~/.config/fauna-scan/nvr-password
${EDITOR:-vi} ~/.config/fauna-scan/classifier-api-key
```

Verify that the files remain private with `stat -c '%a %n'
~/.config/fauna-scan/nvr-password ~/.config/fauna-scan/classifier-api-key` (the
mode should be `600`).

For each password or API key choose exactly one literal, file, or environment
source. File secrets have one trailing newline removed. Do not put credentials
in URLs. `check-config` validates paths, timestamps, TLS policy, lease values,
and secret readability without contacting external services:

```bash
fauna-scan --config ~/.config/fauna-scan/config.toml check-config
```

Use `scheme = "https"` and leave invalid TLS certificates disabled unless the
operator explicitly understands the risk. `start_at` accepts RFC 3339 and is
converted to UTC. Playback URLs are rebased to the configured NVR origin by
default; if rebasing is disabled, use the explicit playback host allowlist.
Classifier Basic authentication uses `username` plus `password_file`; API-key
and Basic authentication may be combined. Configure classifier servers with
one or more `[[classifier.endpoints]]` tables. Each endpoint requires
`base_url` and `model`, and runs one concurrent worker. Set an endpoint's
`enabled = false` to remove it from request rotation without removing its
configuration. Endpoint settings do not inherit from other endpoints;
credentials, timeouts, prompts, and generation settings are resolved
independently. Images are claimed atomically and distributed among enabled
workers. An empty endpoint list disables classification.

## Commands

```bash
fauna-scan --config PATH run
fauna-scan --config PATH web
fauna-scan --config PATH discover
fauna-scan --config PATH download --once
fauna-scan --config PATH scan --once
fauna-scan --config PATH status
fauna-scan --config PATH check-config
fauna-scan --log-level debug --config PATH run
fauna-scan --help
fauna-scan --version
```

Set `[web].enabled = true` to serve the dashboard with `run`, then open the
configured address (the example uses `http://127.0.0.1:8787`). The `web`
command serves the same dashboard against the durable database without
starting downloader or classifier workers. The interface provides time and
camera filters, image and classification detail, live queue/lease monitoring,
the full NVR still-image URL, and an on-demand NVR video recording lookup.
The default listener is loopback-only. Keep it on loopback or place it behind
an authenticated TLS reverse proxy; the initial dashboard does not provide
built-in user authentication.

`run` supervises downloader and scanner pipelines. `download --once` performs
currently due discovery/search/download work; `scan --once` drains eligible
classifications. Configuration and migration errors stop startup. Temporary
network or classifier failures remain durable for retry, and expired leases are
recovered on restart. Completed downloads are not redownloaded, even if their
local file is later deleted.

## Docker Compose deployment

The Compose deployment pulls `git.nerdworks.dev/avranju/fauna-scan:latest` from
the registry. Log in first if the registry requires authentication, then prepare
the container-specific configuration and secret files:

```bash
docker login git.nerdworks.dev
cp config.docker.example.toml config.toml
mkdir -p secrets
install -m 600 /dev/null secrets/nvr-password
install -m 600 /dev/null secrets/classifier-api-key
${EDITOR:-vi} config.toml
${EDITOR:-vi} secrets/nvr-password
${EDITOR:-vi} secrets/classifier-api-key
docker compose config
docker compose pull
docker compose up -d
docker compose logs -f fauna-scan
```

With the example Compose configuration, the dashboard is available only on
the Docker host at `http://127.0.0.1:8787`.

Set `general.database_path` and `general.output_directory` to the container
paths already used by `config.docker.example.toml`. Compose persists those paths
in the `fauna-scan-state` and `fauna-scan-images` named volumes. Classifier and
NVR hostnames must be reachable from the container; `localhost` inside the
container refers to Fauna Scan itself.

To deploy a newly published image, run `docker compose pull` followed by
`docker compose up -d`. Compose stops the old container with a 30-second grace
period and reuses the persistent volumes.

## User systemd service

The repository's `fauna-scan.service` is a user-level example. Its
`ProtectHome=read-only` setting means the database and image directory must be
listed in `ReadWritePaths`; adjust those directives to exactly match the
configured paths before installation. Install and manage it with:

```bash
mkdir -p ~/.config/systemd/user
cp fauna-scan.service ~/.config/systemd/user/
systemd-analyze --user verify ~/.config/systemd/user/fauna-scan.service
systemctl --user daemon-reload
systemctl --user enable --now fauna-scan.service
systemctl --user status fauna-scan.service
journalctl --user -u fauna-scan.service -f
systemctl --user restart fauna-scan.service
systemctl --user stop fauna-scan.service
```

The service uses `Restart=on-failure`, a 30-second stop timeout, and logs to
the user journal. A restart reopens/migrates the same SQLite database, resumes
cursors and pending work, and uses uniqueness plus durable statuses to avoid
duplicate downloads or classifications. Stop sends SIGTERM; the service
should log orderly downloader/scanner termination. Enable lingering or ensure a
user manager/login session is available if it must run without an interactive
login.

## Status, upgrades, and troubleshooting

Use `status` for grouped download and processing counts and inspect the journal
for discovery, completed windows, retries, downloads, classifications, and
shutdown details. If startup fails, run `check-config`, verify secret file
permissions and NVR reachability, and confirm the database/output parents are
writable. With hardening enabled, check that every database and output path is
covered by `ReadWritePaths`.

Before an upgrade, stop the service, install the new binary, and start it
again. SQLite migrations run automatically and failures are reported rather
than ignored; retain a backup of the state database. The acceptance evidence
for this release is in `docs/acceptance-checklist.md`.
