# Fauna Scan — Justfile
# Usage: just <recipe>

bin := "~/.local/bin/fauna-scan"
service_file := "fauna-scan.service"

# ── Build & CI ──────────────────────────────────────────────────────────────

# Format the code
fmt:
	cargo fmt --all -- --check

# Lint with clippy (deny warnings)
lint:
	cargo clippy --all-targets --all-features -- -D warnings

# Run all tests (SQLite only; skips PostgreSQL tests)
test:
	cargo test --all-targets --no-fail-fast

# Run PostgreSQL integration tests against a live server
# Requires FAUNA_SCAN_TEST_POSTGRES_URL to be set
test-postgres:
	FAUNA_SCAN_TEST_POSTGRES_URL="${FAUNA_SCAN_TEST_POSTGRES_URL:?FAUNA_SCAN_TEST_POSTGRES_URL must be set}" cargo test --test postgres_database -- --ignored

# Build debug binary
build: web-build
	cargo build

# Build release binary
release: web-build
	cargo build --release

# Full CI: format check → lint → test → build → install
ci: fmt lint test build
	@echo "All CI checks passed."

# Build release and install binary
install: release
	install -Dm755 target/release/fauna-scan {{bin}}

# Full CI including install
ci-install: ci install
	@echo "Installed to {{bin}}."

# ── Configuration Setup ─────────────────────────────────────────────────────

# Create default directories (config, state, cache, images)
config-init:
	mkdir -p ~/.config/fauna-scan ~/.local/state/fauna-scan ~/Pictures/fauna-scan

# Copy example config to user config
config-copy:
	cp config.example.toml "${XDG_CONFIG_HOME:-$HOME/.config}/fauna-scan/config.toml"

# Create empty secret files with restrictive permissions (600)
config-secrets:
	install -m 600 /dev/null ~/.config/fauna-scan/nvr-password
	install -m 600 /dev/null ~/.config/fauna-scan/classifier-api-key

# Edit secret files securely
config-edit-secrets:
	${EDITOR:-vi} ~/.config/fauna-scan/nvr-password
	${EDITOR:-vi} ~/.config/fauna-scan/classifier-api-key

# Verify secret file permissions are 600
config-check-secrets:
	stat -c '%a %n' ~/.config/fauna-scan/nvr-password ~/.config/fauna-scan/classifier-api-key

# Full setup: init dirs → copy config → create secrets → edit secrets → verify
config-setup: config-init config-copy config-secrets config-edit-secrets config-check-secrets
	@echo "Configuration setup complete."

# Validate configuration without external calls
config-validate:
	fauna-scan --config "${XDG_CONFIG_HOME:-$HOME/.config}/fauna-scan/config.toml" check-config

# ── Running the Service ─────────────────────────────────────────────────────

# Run the main supervisor pipeline
run:
	fauna-scan --config "${XDG_CONFIG_HOME:-$HOME/.config}/fauna-scan/config.toml" run

# Run with debug logging
run-debug:
	fauna-scan --log-level debug --config "${XDG_CONFIG_HOME:-$HOME/.config}/fauna-scan/config.toml" run

# Serve the interface and API without starting workers
web:
	fauna-scan --config "${XDG_CONFIG_HOME:-$HOME/.config}/fauna-scan/config.toml" web

# Discover cameras once
discover:
	fauna-scan --config "${XDG_CONFIG_HOME:-$HOME/.config}/fauna-scan/config.toml" discover

# Download once
download-once:
	fauna-scan --config "${XDG_CONFIG_HOME:-$HOME/.config}/fauna-scan/config.toml" download --once

# Scan/classify once
scan-once:
	fauna-scan --config "${XDG_CONFIG_HOME:-$HOME/.config}/fauna-scan/config.toml" scan --once

# Show status
status:
	fauna-scan --config "${XDG_CONFIG_HOME:-$HOME/.config}/fauna-scan/config.toml" status

# ── Systemd User Service ────────────────────────────────────────────────────

# Install the systemd user service
service-install:
	mkdir -p ~/.config/systemd/user
	cp {{service_file}} ~/.config/systemd/user/

# Verify the service file
service-verify:
	systemd-analyze --user verify ~/.config/systemd/user/{{service_file}}

# Reload systemd, enable and start the service
service-start: service-verify
	systemctl --user daemon-reload
	systemctl --user enable --now {{service_file}}

# Check service status
service-status:
	systemctl --user status {{service_file}}

# View live journal logs
service-logs:
	journalctl --user -u {{service_file}} -f

# Restart the service
service-restart:
	systemctl --user restart {{service_file}}

# Stop the service
service-stop:
	systemctl --user stop {{service_file}}

# Full service setup: install → start → show status
service-enable: service-install service-start service-status
	@echo "Service enabled and running."

# ── Maintenance & Upgrade ───────────────────────────────────────────────────

# Backup the state database before upgrade
backup-state:
	@mkdir -p ~/.local/state/fauna-scan-backup-$(shell date +%Y%m%d%H%M%S)
	cp ~/.local/state/fauna-scan/*.sqlite ~/.local/state/fauna-scan-backup-*/ 2>/dev/null || true
	@echo "State backed up."

# Upgrade: stop service → install new binary → restart
upgrade: service-stop install service-start
	@echo "Upgrade complete."

# ── Help ─────────────────────────────────────────────────────────────────────

# Show all available recipes
help:
	@just --list

# Build the embedded React application (Node.js 24+).
web-build:
    cd web && npm ci && npm run format:check && npm run build

# Vite development server; proxies /api to the local web service on port 8787.
web-dev:
    cd web && npm run dev

# Browser workflow and accessibility tests (set CHROMIUM_PATH if needed).
web-test:
    cd web && npm test
