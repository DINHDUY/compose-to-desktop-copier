#!/usr/bin/env bash
# Install the macOS or Linux toolchain used by `make dev`.
# Docker Desktop is never installed. A working Docker engine is kept.
# Podman is installed when no Docker engine is available.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 1

MIN_RUST="$(awk '/^rust-version/ { gsub(/"/, "", $3); print $3; exit }' "$ROOT/src-tauri/Cargo.toml")"
MIN_COMPOSE="2.20.0"
failures=()

fail() {
  failures+=("$1")
  echo "init: $1" >&2
}

version_ge() {
  local a="${1#v}" b="${2#v}"
  local a1 a2 a3 b1 b2 b3
  IFS=. read -r a1 a2 a3 _ <<EOF
$a
EOF
  IFS=. read -r b1 b2 b3 _ <<EOF
$b
EOF
  a1="${a1:-0}"; a2="${a2:-0}"; a3="${a3:-0}"
  b1="${b1:-0}"; b2="${b2:-0}"; b3="${b3:-0}"
  a1="${a1%%[!0-9]*}"; a2="${a2%%[!0-9]*}"; a3="${a3%%[!0-9]*}"
  b1="${b1%%[!0-9]*}"; b2="${b2%%[!0-9]*}"; b3="${b3%%[!0-9]*}"
  a1="${a1:-0}"; a2="${a2:-0}"; a3="${a3:-0}"
  b1="${b1:-0}"; b2="${b2:-0}"; b3="${b3:-0}"
  if [ "$a1" -ne "$b1" ]; then [ "$a1" -gt "$b1" ]; return; fi
  if [ "$a2" -ne "$b2" ]; then [ "$a2" -gt "$b2" ]; return; fi
  [ "$a3" -ge "$b3" ]
}

run_root() {
  if [ "$(id -u)" -eq 0 ]; then
    "$@"
  elif command -v sudo >/dev/null 2>&1; then
    sudo "$@"
  else
    fail "root is required to run: $*"
    return 1
  fi
}

prepend_path() {
  case ":$PATH:" in
    *":$1:"*) ;;
    *) PATH="$1:$PATH" ;;
  esac
  export PATH
}

prepare_path() {
  if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
  prepend_path "$HOME/.local/bin"
  prepend_path "$HOME/.cargo/bin"
  if [ -x /opt/homebrew/bin/brew ]; then
    eval "$(/opt/homebrew/bin/brew shellenv)"
  elif [ -x /usr/local/bin/brew ]; then
    eval "$(/usr/local/bin/brew shellenv)"
  fi
}

file_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print tolower($1)}'
  else
    shasum -a 256 "$1" | awk '{print tolower($1)}'
  fi
}

compose_pinned_sha256() {
  local asset="$1" pinned=""
  if command -v python3 >/dev/null 2>&1; then
    pinned="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["compose"]["sha256"][sys.argv[2]])' \
      "$ROOT/installer/versions.json" "$asset" 2>/dev/null || true)"
  fi
  if [ -z "$pinned" ]; then
    pinned="$(awk -v asset="$asset" '
      index($0, "\"" asset "\":") {
        if (match($0, /[A-Fa-f0-9]{64}/)) {
          print tolower(substr($0, RSTART, RLENGTH))
          exit
        }
      }
    ' "$ROOT/installer/versions.json")"
  fi
  printf '%s\n' "$(printf '%s' "$pinned" | tr '[:upper:]' '[:lower:]')"
}

compose_release_version() {
  if command -v python3 >/dev/null 2>&1; then
    python3 -c 'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["compose"]["version"])' \
      "$ROOT/installer/versions.json"
    return
  fi
  awk '
    /"compose"[[:space:]]*:/ { capture=1 }
    capture && /"version"/ {
      if (match($0, /[0-9]+\.[0-9]+\.[0-9]+/)) {
        print substr($0, RSTART, RLENGTH)
        exit
      }
    }
  ' "$ROOT/installer/versions.json"
}

host_compose_asset() {
  local os arch
  case "$(uname -s)" in
    Darwin) os="darwin" ;;
    Linux) os="linux" ;;
    *)
      fail "unsupported operating system: $(uname -s)"
      return 1
      ;;
  esac
  case "$(uname -m)" in
    x86_64 | amd64) arch="x86_64" ;;
    arm64 | aarch64) arch="aarch64" ;;
    *)
      fail "unsupported architecture: $(uname -m)"
      return 1
      ;;
  esac
  printf 'docker-compose-%s-%s\n' "$os" "$arch"
}

compose_file_ok() {
  local bin="$1" version size
  [ -n "$bin" ] && [ -f "$bin" ] && [ -x "$bin" ] || return 1
  size="$(wc -c < "$bin" | tr -d '[:space:]')"
  [ "${size:-0}" -ge 1000000 ] || return 1
  version="$(compose_bin_version "$bin")"
  [ -n "$version" ] || return 1
  version_ge "$version" "$MIN_COMPOSE"
}

find_compose_bin() {
  local candidate found=""
  for candidate in \
    "$HOME/.local/bin/docker-compose" \
    "$(command -v docker-compose 2>/dev/null || true)" \
    /opt/homebrew/bin/docker-compose \
    /usr/local/bin/docker-compose \
    /usr/libexec/docker/cli-plugins/docker-compose \
    /usr/lib/docker/cli-plugins/docker-compose
  do
    [ -n "$candidate" ] || continue
    if compose_file_ok "$candidate"; then
      printf '%s\n' "$candidate"
      return 0
    fi
    if [ -z "$found" ] && [ -x "$candidate" ]; then
      found="$candidate"
    fi
  done
  if [ -n "$found" ]; then
    printf '%s\n' "$found"
    return 0
  fi
  return 1
}

compose_bin_version() {
  local bin="$1" raw
  raw="$("$bin" version --short 2>/dev/null || true)"
  raw="${raw%%$'\r'*}"
  raw="${raw#v}"
  printf '%s\n' "${raw%% *}"
}

compose_is_new_enough() {
  local bin
  bin="$(find_compose_bin 2>/dev/null || true)"
  compose_file_ok "$bin"
}

link_compose_bin() {
  local source="$1"
  mkdir -p "$HOME/.local/bin"
  ln -sfn "$source" "$HOME/.local/bin/docker-compose"
  prepend_path "$HOME/.local/bin"
}

download_pinned_compose() {
  local version asset url checksum_url checksum_file expected pinned dest actual partial
  version="$(compose_release_version)"
  asset="$(host_compose_asset)" || return 1
  pinned="$(compose_pinned_sha256 "$asset")"
  if [ -z "$pinned" ]; then
    fail "no pinned sha256 for Compose ${asset}"
    return 1
  fi
  url="https://github.com/docker/compose/releases/download/v${version}/${asset}"
  checksum_url="https://github.com/docker/compose/releases/download/v${version}/checksums.txt"
  checksum_file="$(mktemp)"
  partial="$(mktemp)"
  echo "Downloading Compose ${version} (${asset})"
  if ! curl --proto '=https' --tlsv1.2 -fsSL "$checksum_url" -o "$checksum_file"; then
    rm -f "$checksum_file" "$partial"
    fail "could not download Compose checksums"
    return 1
  fi
  expected="$(grep -F "$asset" "$checksum_file" | head -n 1 | grep -Eo '[A-Fa-f0-9]{64}' | head -n 1 || true)"
  expected="$(printf '%s' "$expected" | tr '[:upper:]' '[:lower:]')"
  if [ -z "$expected" ]; then
    rm -f "$checksum_file" "$partial"
    fail "no checksum for ${asset} in Compose ${version} checksums.txt"
    return 1
  fi
  if [ "$expected" != "$pinned" ]; then
    rm -f "$checksum_file" "$partial"
    fail "upstream checksum for Compose ${asset} does not match the pinned sha256"
    return 1
  fi
  dest="$HOME/.local/bin/docker-compose"
  if [ -x "$dest" ]; then
    actual="$(file_sha256 "$dest")"
    if [ "$actual" = "$expected" ]; then
      rm -f "$checksum_file" "$partial"
      echo "Compose ${version} is already installed"
      return 0
    fi
  fi
  if ! curl --proto '=https' --tlsv1.2 -fsSL "$url" -o "$partial"; then
    rm -f "$checksum_file" "$partial"
    fail "could not download Compose ${asset}"
    return 1
  fi
  actual="$(file_sha256 "$partial")"
  if [ "$actual" != "$expected" ]; then
    rm -f "$checksum_file" "$partial"
    fail "checksum mismatch for Compose ${asset}"
    return 1
  fi
  mkdir -p "$HOME/.local/bin"
  mv "$partial" "$dest"
  chmod 755 "$dest"
  rm -f "$checksum_file"
  prepend_path "$HOME/.local/bin"
}

install_macos_tools() {
  if ! xcode-select -p >/dev/null 2>&1; then
    xcode-select --install >/dev/null 2>&1 || true
    fail "Xcode Command Line Tools are not installed. Finish the installer dialog, then rerun make init."
  fi
}

install_linux_packages() {
  local id like
  id="$(. /etc/os-release && printf '%s' "$ID")"
  like="$(. /etc/os-release && printf '%s' "${ID_LIKE:-}")"
  if [ "$id" = "debian" ] || [ "$id" = "ubuntu" ] || [ "$id" = "linuxmint" ] || [ "$id" = "pop" ] || [[ "$like" == *debian* ]]; then
    run_root apt-get update || fail "apt-get update failed"
    run_root apt-get install -y \
      build-essential pkg-config curl wget file libssl-dev \
      libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev \
      patchelf libxdo-dev ca-certificates \
      || fail "could not install the Debian/Ubuntu WebView packages"
    return
  fi
  if [ "$id" = "fedora" ] || [ "$id" = "rhel" ] || [ "$id" = "centos" ] || [ "$id" = "rocky" ] || [ "$id" = "almalinux" ] || [[ "$like" == *fedora* ]] || [[ "$like" == *rhel* ]]; then
    run_root dnf install -y \
      gcc gcc-c++ make pkgconf-pkg-config webkit2gtk4.1-devel openssl-devel \
      curl wget file libappindicator-gtk3-devel librsvg2-devel libxdo-devel \
      patchelf ca-certificates \
      || fail "could not install the Fedora/RHEL WebView packages"
    return
  fi
  if [ "$id" = "arch" ] || [ "$id" = "manjaro" ] || [ "$id" = "endeavouros" ] || [[ "$like" == *arch* ]]; then
    run_root pacman -Sy --needed --noconfirm \
      base-devel curl wget file openssl webkit2gtk-4.1 \
      appmenu-gtk-module libappindicator-gtk3 librsvg xdotool patchelf ca-certificates \
      || fail "could not install the Arch WebView packages"
    return
  fi
  fail "unsupported Linux distribution '${id}'. Install WebKitGTK 4.1, a C compiler, pkg-config, curl, and patchelf, then rerun make init."
  return 1
}

ensure_rust() {
  if ! command -v rustup >/dev/null 2>&1 && { ! command -v rustc >/dev/null 2>&1 || ! version_ge "$(rustc --version | awk '{print $2}')" "$MIN_RUST"; }; then
    echo "Installing rustup"
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
  if command -v rustup >/dev/null 2>&1; then
    if ! command -v rustc >/dev/null 2>&1 || ! version_ge "$(rustc --version | awk '{print $2}')" "$MIN_RUST"; then
      rustup toolchain install stable
      rustup default stable
    fi
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    fail "cargo is not on PATH. Open a new shell and rerun make init."
    return 1
  fi
  if ! command -v rustc >/dev/null 2>&1 || ! version_ge "$(rustc --version | awk '{print $2}')" "$MIN_RUST"; then
    fail "rustc $(rustc --version 2>/dev/null | awk '{print $2}') is older than ${MIN_RUST}."
    return 1
  fi
}

ensure_tauri_cli() {
  local version major
  if ! command -v cargo >/dev/null 2>&1; then
    return 1
  fi
  version="$(cargo tauri --version 2>/dev/null | awk '{print $2}' || true)"
  major="${version%%.*}"
  if [ "$major" != "2" ]; then
    echo "Installing tauri-cli ^2"
    cargo install tauri-cli --version '^2' --force
  fi
}

docker_desktop_installed() {
  case "$(uname -s)" in
    Darwin)
      [ -d /Applications/Docker.app ] || [ -d "$HOME/Applications/Docker.app" ]
      ;;
    Linux)
      [ -d /opt/docker-desktop ] || command -v docker-desktop >/dev/null 2>&1
      ;;
    *)
      return 1
      ;;
  esac
}

docker_permission_denied() {
  local output
  output="$(docker info 2>&1 || true)"
  printf '%s' "$output" | grep -qi 'permission denied'
}

write_runtime() {
  printf '%s\n' "$1" > "$ROOT/.container-runtime"
  echo "Wrote .container-runtime ($1)"
}

install_podman_package() {
  local id like
  case "$(uname -s)" in
    Darwin)
      if ! command -v brew >/dev/null 2>&1; then
        fail "Homebrew is required to install Podman. Install it from https://brew.sh and rerun make init."
        return 1
      fi
      if ! brew list --formula podman >/dev/null 2>&1; then
        brew install podman || {
          fail "brew install podman failed"
          return 1
        }
      fi
      ;;
    Linux)
      if command -v podman >/dev/null 2>&1; then
        return 0
      fi
      id="$(. /etc/os-release && printf '%s' "$ID")"
      like="$(. /etc/os-release && printf '%s' "${ID_LIKE:-}")"
      if [ "$id" = "debian" ] || [ "$id" = "ubuntu" ] || [ "$id" = "linuxmint" ] || [ "$id" = "pop" ] || [[ "$like" == *debian* ]]; then
        run_root apt-get install -y podman || {
          fail "could not install podman"
          return 1
        }
      elif [ "$id" = "fedora" ] || [ "$id" = "rhel" ] || [ "$id" = "centos" ] || [ "$id" = "rocky" ] || [ "$id" = "almalinux" ] || [[ "$like" == *fedora* ]] || [[ "$like" == *rhel* ]]; then
        run_root dnf install -y podman || {
          fail "could not install podman"
          return 1
        }
      elif [ "$id" = "arch" ] || [ "$id" = "manjaro" ] || [ "$id" = "endeavouros" ] || [[ "$like" == *arch* ]]; then
        run_root pacman -Sy --needed --noconfirm podman || {
          fail "could not install podman"
          return 1
        }
      else
        fail "install Podman with the ${id} package manager, then rerun make init."
        return 1
      fi
      ;;
  esac
}

start_podman_machine() {
  if [ "$(uname -s)" != "Darwin" ]; then
    return 0
  fi
  if ! podman machine inspect >/dev/null 2>&1; then
    echo "Initializing the Podman machine"
    podman machine init
  fi
  if podman info >/dev/null 2>&1; then
    return 0
  fi
  echo "Starting the Podman machine"
  podman machine start
}

ensure_compose_provider() {
  local bin
  if compose_is_new_enough; then
    bin="$(find_compose_bin)"
    case "$bin" in
      "$HOME/.local/bin/docker-compose") ;;
      *)
        if ! command -v docker-compose >/dev/null 2>&1; then
          link_compose_bin "$bin"
        fi
        ;;
    esac
    return 0
  fi
  case "$(uname -s)" in
    Darwin)
      if command -v brew >/dev/null 2>&1 && ! brew list --formula docker-compose >/dev/null 2>&1; then
        brew install docker-compose || true
      fi
      ;;
    Linux)
      if [ -r /etc/os-release ]; then
        local id like
        id="$(. /etc/os-release && printf '%s' "$ID")"
        like="$(. /etc/os-release && printf '%s' "${ID_LIKE:-}")"
        if [ "$id" = "debian" ] || [ "$id" = "ubuntu" ] || [[ "$like" == *debian* ]]; then
          run_root apt-get install -y docker-compose-v2 || run_root apt-get install -y docker-compose || true
        elif [ "$id" = "fedora" ] || [[ "$like" == *fedora* ]] || [[ "$like" == *rhel* ]]; then
          run_root dnf install -y docker-compose || run_root dnf install -y docker-compose-plugin || true
        elif [ "$id" = "arch" ] || [[ "$like" == *arch* ]]; then
          run_root pacman -Sy --needed --noconfirm docker-compose || true
        fi
      fi
      ;;
  esac
  if compose_is_new_enough; then
    bin="$(find_compose_bin)"
    if ! command -v docker-compose >/dev/null 2>&1; then
      link_compose_bin "$bin"
    fi
    return 0
  fi
  download_pinned_compose
}

select_engine() {
  if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
    write_runtime docker
    ENGINE="docker"
    return 0
  fi
  if docker_desktop_installed; then
    write_runtime docker
    ENGINE="docker"
    fail "Docker Desktop is installed and the daemon is stopped. Start Docker Desktop, then rerun make dev."
    return 0
  fi
  if command -v docker >/dev/null 2>&1 && docker_permission_denied; then
    write_runtime docker
    ENGINE="docker"
    fail "Docker is installed and this user cannot access the daemon. Open a new shell after your group membership updates, then rerun make dev."
    return 0
  fi

  echo "Docker is not available. Installing Podman."
  install_podman_package || return 1
  start_podman_machine || fail "Podman is installed and the machine did not start."
  ensure_compose_provider || fail "Compose ${MIN_COMPOSE} or newer is not installed."
  write_runtime podman
  ENGINE="podman"
}

report_cmd() {
  local label="$1"
  shift
  local output
  if output="$("$@" 2>/dev/null)"; then
    output="${output%%$'\n'*}"
    printf '  %-14s %s\n' "$label" "$output"
  else
    printf '  %-14s missing\n' "$label"
    return 1
  fi
}

finish() {
  local engine="${ENGINE:-unknown}"
  echo ""
  echo "Development environment"
  echo "  engine         ${engine}"
  report_cmd "rustc" rustc --version || fail "rustc ${MIN_RUST} or newer is required."
  report_cmd "cargo tauri" cargo tauri --version || fail "tauri-cli ^2 is required."
  case "$(uname -s)" in
    Darwin)
      if ! xcode-select -p >/dev/null 2>&1; then
        fail "Xcode Command Line Tools are required."
      fi
      ;;
    Linux)
      if ! pkg-config --exists webkit2gtk-4.1; then
        fail "webkit2gtk-4.1 is required."
      fi
      ;;
  esac
  if [ "$engine" = "docker" ]; then
    report_cmd "docker" docker --version || fail "docker is required."
    if docker info >/dev/null 2>&1; then
      report_cmd "compose" docker compose version --short || fail "docker compose is required."
      local compose_version
      compose_version="$(docker compose version --short 2>/dev/null || true)"
      compose_version="${compose_version#v}"
      if [ -z "$compose_version" ] || ! version_ge "$compose_version" "$MIN_COMPOSE"; then
        fail "Docker Compose ${compose_version:-unknown} is older than ${MIN_COMPOSE}."
      fi
    fi
  elif [ "$engine" = "podman" ]; then
    report_cmd "podman" podman --version || fail "podman is required."
    if ! podman info >/dev/null 2>&1; then
      fail "Podman is installed and the engine is not running."
    fi
    local bin compose_version
    bin="$(find_compose_bin 2>/dev/null || true)"
    if [ -n "$bin" ]; then
      compose_version="$(compose_bin_version "$bin")"
      printf '  %-14s %s (%s)\n' "compose" "$compose_version" "$bin"
      if ! compose_file_ok "$bin"; then
        fail "Compose at ${bin} must be ${MIN_COMPOSE} or newer and at least 1 MB."
      fi
    else
      fail "docker-compose ${MIN_COMPOSE} or newer is required for Podman."
    fi
  fi
  if [ -f "$ROOT/.env" ]; then
    printf '  %-14s %s\n' ".env" ".env"
  fi
  echo ""
  if [ "${#failures[@]}" -gt 0 ]; then
    echo "Still missing:"
    local item
    for item in "${failures[@]}"; do
      echo "  - $item"
    done
    exit 1
  fi
  echo "Ready for make dev."
}

prepare_path
case "$(uname -s)" in
  Darwin) install_macos_tools ;;
  Linux) install_linux_packages ;;
  *)
    echo "init: unsupported operating system $(uname -s). On Windows run make init from Git Bash or MSYS2." >&2
    exit 1
    ;;
esac
ensure_rust || true
ensure_tauri_cli || true
select_engine || true
finish
