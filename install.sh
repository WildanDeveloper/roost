#!/bin/bash

set -e

# roost installer — mirrors the community pterodactyl-installer flow for Wings:
#   bash <(curl -s https://raw.githubusercontent.com/WildanDeveloper/roost/master/install.sh)
#
# Headless mode (non-interactive):
#   ROOST_SKIP_DOCKER=true      do not install/modify Docker
#   ROOST_SKIP_CONFIGURE=true   skip the interactive panel configuration step
#   ROOST_PANEL_URL=            panel base URL (implies configure)
#   ROOST_PANEL_TOKEN=          panel application API key
#   ROOST_NODE_ID=              numeric node id
#   ROOST_AUTO_START=true       start the service immediately after configuration

ROOST_GITHUB_REPO="WildanDeveloper/roost"
ROOST_DL_BASE_URL="https://github.com/${ROOST_GITHUB_REPO}/releases/latest/download/roost_linux_"
ROOST_BIN="/usr/local/bin/roost"
ROOST_CONFIG_PATH="/etc/pterodactyl/config.yml"

# pretty output helpers

output() { echo "* $*"; }
success() { echo "* [SUCCESS] $*"; }
error() { echo "* [ERROR] $*" >&2; }
warning() { echo "* [WARNING] $*" >&2; }

# ------------------------------ pre-checks ------------------------------- #

check_root() {
  if [ "$(id -u)" != "0" ]; then
    error "This installer must be run as root (try: sudo bash <(curl -s ...))"
    exit 1
  fi
}

check_curl() {
  if ! [ -x "$(command -v curl)" ]; then
    error "curl is required. Install it with apt (Debian/Ubuntu) or dnf/yum."
    exit 1
  fi
}

# detection

detect_os() {
  if [ ! -f /etc/os-release ]; then
    error "Unsupported OS: /etc/os-release not found"
    exit 1
  fi
  # shellcheck source=/dev/null
  . /etc/os-release
  OS="${ID:-unknown}"
  OS_VER="${VERSION_ID:-unknown}"

  case "$OS" in
  ubuntu | debian | rocky | almalinux | centos | fedora)
    output "Detected $OS $OS_VER"
    ;;
  *)
    warning "Unsupported distribution ($OS); continuing, but Docker installation will be skipped."
    ROOST_SKIP_DOCKER=true
    ;;
  esac
}

detect_arch() {
  case "$(uname -m)" in
  x86_64) ARCH="amd64" ;;
  aarch64 | arm64) ARCH="arm64" ;;
  *)
    error "Unsupported architecture: $(uname -m)"
    exit 1
    ;;
  esac
  output "Architecture: $ARCH"
}

# docker

docker_installed() {
  command -v docker >/dev/null 2>&1 && systemctl is-active docker >/dev/null 2>&1
}

install_docker() {
  if docker_installed; then
    output "Docker is already installed and running"
    return 0
  fi

  if [ "${ROOST_SKIP_DOCKER:-false}" == "true" ]; then
    warning "ROOST_SKIP_DOCKER=true; skipping Docker installation"
    return 0
  fi

  output "Installing Docker (official convenience script)..."
  curl -fsSL https://get.docker.com | sh

  systemctl enable docker
  systemctl start docker
  success "Docker installed"
}

# binary

download_binary() {
  output "Downloading roost (linux_$ARCH)..."

  if ! curl -fsSL -o "$ROOST_BIN.tmp" "${ROOST_DL_BASE_URL}${ARCH}"; then
    error "Could not download the roost binary."
    error "Is there a published release? Check: https://github.com/${ROOST_GITHUB_REPO}/releases"
    exit 1
  fi
  if ! curl -fsSL -o "$ROOST_BIN.tmp.sha256" "${ROOST_DL_BASE_URL}${ARCH}.sha256"; then
    warning "No sha256 checksum published for this release; skipping verification"
    mv "$ROOST_BIN.tmp" "$ROOST_BIN"
  else
    echo "$(cat "$ROOST_BIN.tmp.sha256" | awk '{print $1}')  $ROOST_BIN.tmp" | sha256sum -c - >/dev/null 2>&1 ||
      {
        error "Checksum verification failed! The download may be corrupted or tampered with."
        rm -f "$ROOST_BIN.tmp" "$ROOST_BIN.tmp.sha256"
        exit 1
      }
    mv "$ROOST_BIN.tmp" "$ROOST_BIN"
    rm -f "$ROOST_BIN.tmp.sha256"
  fi

  chmod 755 "$ROOST_BIN"
  success "roost installed to $ROOST_BIN"
}

# directories + user

# conflict detection: never coexist with a live Wings (or another roost) on
# the same node — both daemons would fight over the same Docker containers,
# data directories and ports.
detect_conflicts() {
  CONFLICTS=""
  EXISTING_ROOST=false

  if [ -x /usr/local/bin/wings ] || command -v wings >/dev/null 2>&1; then
    CONFLICTS="$CONFLICTS
  - wings binary found ($(command -v wings 2>/dev/null || echo /usr/local/bin/wings))"
  fi

  if systemctl list-unit-files 2>/dev/null | grep -q "^wings.service"; then
    CONFLICTS="$CONFLICTS
  - wings systemd service is installed"
    if systemctl is-active --quiet wings 2>/dev/null; then
      CONFLICTS="$CONFLICTS (RUNNING)"
    fi
  fi

  if pgrep -x wings >/dev/null 2>&1; then
    CONFLICTS="$CONFLICTS
  - wings process is running (pid $(pgrep -x wings | tr '\n' ' '))"
  fi

  # port check: catches other daemons (or anything else) squatting on the
  # wings-compatible ports even when no wings binary is found
  if command -v ss >/dev/null 2>&1; then
    for port in 8080 2022; do
      if ss -tlnH 2>/dev/null | awk '{print $4}' | grep -qE "[:.]${port}$"; then
        CONFLICTS="$CONFLICTS
  - port $port is already in use"
      fi
    done
  fi

  # upgrade path: only a roost service marks a previous roost install (the
  # config path is shared with wings, so it proves nothing on its own)
  if systemctl list-unit-files 2>/dev/null | grep -q "^roost.service"; then
    EXISTING_ROOST=true
  fi

  if [ -n "$CONFLICTS" ]; then
    if [ "$EXISTING_ROOST" != "true" ]; then
      error "This node appears to already run a Pterodactyl daemon:"
      echo "$CONFLICTS"
      echo ""
      error "roost and Wings cannot share a node: they use the same Docker"
      error "containers, data directories and ports (8080/2022)."
      error "Stop and disable Wings first (systemctl disable --now wings),"
      error "or set ROOST_FORCE=true to install anyway (NOT recommended)."
      if [ "${ROOST_FORCE:-false}" != "true" ]; then
        exit 1
      fi
      warning "ROOST_FORCE=true — installing anyway"
    else
      warning "Found a roost service AND Wings artifacts on this node;"
      warning "review the findings above before continuing."
      echo "$CONFLICTS"
    fi
  fi

  if [ "$EXISTING_ROOST" == "true" ]; then
    output "Existing roost installation detected; upgrading binary and service"
    output "(configuration in $ROOST_CONFIG_PATH is kept)"
    systemctl stop roost 2>/dev/null || true
  fi
}

create_directories() {
  output "Creating directories..."
  mkdir -p /etc/pterodactyl
  mkdir -p /var/log/pterodactyl
  mkdir -p /var/lib/pterodactyl/volumes
  mkdir -p /var/lib/pterodactyl/archives
  mkdir -p /var/lib/pterodactyl/backups
  mkdir -p /tmp/pterodactyl
  success "Directories created"
}

# systemd

install_service() {
  output "Installing systemd service..."

  cat >/etc/systemd/system/roost.service <<'EOF'
[Unit]
Description=Pterodactyl Roost Daemon (Wings-compatible)
After=docker.service
Requires=docker.service
PartOf=docker.service

[Service]
User=root
WorkingDirectory=/etc/pterodactyl
LimitNOFILE=4096
Environment=ROOST_CONFIG=/etc/pterodactyl/config.yml
ExecStart=/usr/local/bin/roost
Restart=on-failure
StartLimitInterval=180
StartLimitBurst=30
RestartSec=5s

[Install]
WantedBy=multi-user.target
EOF

  systemctl daemon-reload
  systemctl enable roost
  success "Installed roost.service (enabled at boot)"
}

# configuration

configure_node() {
  [ "${ROOST_SKIP_CONFIGURE:-false}" == "true" ] && return 0

  if [ -f "$ROOST_CONFIG_PATH" ] && [ "${ROOST_RECONFIGURE:-false}" != "true" ]; then
    output "Using existing configuration at $ROOST_CONFIG_PATH"
    output "(set ROOST_RECONFIGURE=true to re-register the node)"
    CONFIGURED=true
    return 0
  fi

  echo ""
  output "The daemon needs the node credentials from your panel."
  output "Admin area -> Nodes -> (your node) -> Configuration."
  echo ""

  PANEL_URL="${ROOST_PANEL_URL:-}"
  PANEL_TOKEN="${ROOST_PANEL_TOKEN:-}"
  NODE_ID="${ROOST_NODE_ID:-}"

  if [ -z "$PANEL_URL" ]; then
    echo -n "* Panel URL (e.g. https://panel.example.com): "
    read -r PANEL_URL
  fi
  if [ -z "$PANEL_TOKEN" ]; then
    echo -n "* Application API key: "
    read -r PANEL_TOKEN
  fi
  if [ -z "$NODE_ID" ]; then
    echo -n "* Node ID: "
    read -r NODE_ID
  fi

  if [ -z "$PANEL_URL" ] || [ -z "$PANEL_TOKEN" ] || [ -z "$NODE_ID" ]; then
    warning "Incomplete credentials; skipping configuration. Re-run the installer or use:"
    warning "  roost configure --panel-url <url> --token <key> --node <id>"
    return 0
  fi

  EXTRA_FLAGS=""
  if [ -n "${ROOST_ALLOW_INSECURE:-}" ]; then
    EXTRA_FLAGS="--allow-insecure"
  fi

  if "$ROOST_BIN" configure \
    --panel-url "$PANEL_URL" \
    --token "$PANEL_TOKEN" \
    --node "$NODE_ID" \
    --config-path "$ROOST_CONFIG_PATH" \
    --override $EXTRA_FLAGS; then
    success "Configuration written to $ROOST_CONFIG_PATH"
    CONFIGURED=true
  else
    warning "Configuration failed. Fix the credentials and re-run 'roost configure'."
  fi
}

start_service() {
  if [ "${CONFIGURED:-false}" != "true" ] && [ -z "${ROOST_AUTO_START}" ]; then
    warning "Service not started: no configuration yet. After configuring, run:"
    warning "  systemctl start roost"
    return 0
  fi

  output "Starting roost..."
  systemctl restart roost
  sleep 2
  systemctl --no-pager status roost || true
  success "roost is running!"
}

# main

main() {
  output "roost installer (Wings-compatible Pterodactyl daemon)"
  output "https://github.com/${ROOST_GITHUB_REPO}"
  echo ""

  check_root
  check_curl
  detect_os
  detect_arch
  detect_conflicts
  install_docker
  download_binary
  create_directories
  install_service
  configure_node
  start_service

  echo ""
  success "Installation complete."
  echo ""
  echo "  config:     $ROOST_CONFIG_PATH"
  echo "  service:    systemctl {start,stop,status} roost"
  echo "  logs:       journalctl -u roost -f"
  echo "  diagnostics: roost diagnostics"
  echo ""
}

main
