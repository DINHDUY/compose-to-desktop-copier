#!/usr/bin/env bash
# Render this Copier template and test the generated projects.
# The check copies the working tree into a temporary git repository and tags
# v1.0.0, because Copier updates require a git-tracked template and a recorded
# commit.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
COPIER="${COPIER:-copier}"
WORK="$(mktemp -d)"
WORK="$(cd "$WORK" && pwd -P)"

cleanup() {
  rm -rf "$WORK"
}
trap cleanup EXIT

git_commit() {
  local dir="$1"
  local message="$2"
  git -C "$dir" add -A
  git -C "$dir" \
    -c user.name=template \
    -c user.email=template@example.com \
    -c commit.gpgsign=false \
    commit -q -m "$message"
}

require_lint() {
  local dir="$1"
  local log
  log="$(make -C "$dir" stack-check 2>&1)"
  printf '%s\n' "$log"
  printf '%s\n' "$log" | grep -q 'repo_stacks_pass_lint ... ok'
}

SRC="$WORK/src"
mkdir -p "$SRC"
rsync -a \
  --exclude .git \
  --exclude .rendered \
  --exclude .container-runtime \
  --exclude .env \
  --exclude target \
  --exclude installer/cache \
  "$ROOT/" "$SRC/"
git -C "$SRC" init -q -b main
git_commit "$SRC" "Template 1.0.0"
git -C "$SRC" tag v1.0.0

echo "Rendering default answers"
"$COPIER" copy "$SRC" "$WORK/default" --defaults --vcs-ref v1.0.0

default="$WORK/default"
grep -q '_src_path:' "$default/.copier-answers.yml"
grep -q '_commit:' "$default/.copier-answers.yml"
grep -q 'product_name = "compose-to-desktop-copier"' "$default/shell.toml"
grep -q 'project_name = "compose-to-desktop-copier"' "$default/shell.toml"
grep -q 'health_url = "http://127.0.0.1:3000/"' "$default/shell.toml"
grep -q 'secret_keys = \["POSTGRES_PASSWORD"\]' "$default/shell.toml"
grep -q 'name = "compose-to-desktop-copier"' "$default/src-tauri/Cargo.toml"
grep -q 'name = "compose_to_desktop_copier_lib"' "$default/src-tauri/Cargo.toml"
grep -q 'compose_to_desktop_copier_lib::run()' "$default/src-tauri/src/main.rs"
grep -q '"productName": "compose-to-desktop-copier"' "$default/src-tauri/tauri.conf.json"
grep -q '"identifier": "com.example.desktop"' "$default/src-tauri/tauri.conf.json"
grep -q 'compose-to-desktop-copier needs Podman' "$default/src-tauri/windows/hooks.nsh"
grep -q 'profiles: \[backend\]' "$default/docker-compose.yml"
grep -q 'profiles: \[database\]' "$default/docker-compose.yml"
grep -q 'API_IMAGE=nginx:alpine' "$default/.env.example"
grep -q 'POSTGRES_PASSWORD=' "$default/.env.example"
grep -q 'Copyright compose-to-desktop-copier' "$default/LICENSE"
test -f "$default/src-tauri/icons/icon.ico"
test -f "$default/scripts/init-unix.sh"
test -x "$default/scripts/init-unix.sh"
test ! -e "$default/.container-runtime"
test ! -d "$default/examples"
test ! -d "$default/src-tauri/target"
if grep -R -E '\{\{ *(product_name|project_slug|ui_port|bundle_identifier|description|include_|license)|\{%' "$default" >/dev/null; then
  echo "Jinja markers leaked into the default render" >&2
  exit 1
fi

echo "Testing the default render"
(
  cd "$default"
  cargo test --locked --manifest-path src-tauri/Cargo.toml
)
require_lint "$default"

echo "Linting the Superset reference stack"
mkdir -p "$WORK/with-example"
cp -a "$default/." "$WORK/with-example/"
mkdir -p "$WORK/with-example/examples"
cp -a "$ROOT/examples/superset" "$WORK/with-example/examples/superset"
require_lint "$WORK/with-example"

echo "Rendering a frontend-only variant"
cat > "$WORK/variant.yml" <<'EOF'
product_name: Acme Desk
project_slug: acme-desk
bundle_identifier: com.example.acmedesk
description: Acme desktop shell
ui_port: 4010
include_backend: false
include_database: false
license: MIT
EOF
"$COPIER" copy "$SRC" "$WORK/variant" --data-file "$WORK/variant.yml" --defaults --vcs-ref v1.0.0
variant="$WORK/variant"
grep -q 'product_name = "Acme Desk"' "$variant/shell.toml"
grep -q 'project_name = "acme-desk"' "$variant/shell.toml"
grep -q 'health_url = "http://127.0.0.1:4010/"' "$variant/shell.toml"
grep -q 'secret_keys = \[\]' "$variant/shell.toml"
grep -q 'name = "acme-desk"' "$variant/src-tauri/Cargo.toml"
grep -q 'name = "acme_desk_lib"' "$variant/src-tauri/Cargo.toml"
grep -q 'acme_desk_lib::run()' "$variant/src-tauri/src/main.rs"
grep -q '"identifier": "com.example.acmedesk"' "$variant/src-tauri/tauri.conf.json"
grep -q 'http://127.0.0.1:4010' "$variant/src-tauri/tauri.conf.json"
grep -q 'Acme Desk needs Podman' "$variant/src-tauri/windows/hooks.nsh"
grep -q 'MIT License' "$variant/LICENSE"
if grep -q 'profiles: \[backend\]' "$variant/docker-compose.yml"; then
  echo "variant compose still has the api profile" >&2
  exit 1
fi
if grep -q 'profiles: \[database\]' "$variant/docker-compose.yml"; then
  echo "variant compose still has the database profile" >&2
  exit 1
fi
if grep -q 'API_IMAGE=' "$variant/.env.example" || grep -q 'POSTGRES_' "$variant/.env.example"; then
  echo "variant .env.example still has optional services" >&2
  exit 1
fi
(
  cd "$variant"
  cargo test --manifest-path src-tauri/Cargo.toml
)
require_lint "$variant"

echo "Rejecting an invalid slug"
if "$COPIER" copy "$SRC" "$WORK/invalid" --defaults --vcs-ref v1.0.0 --data project_slug=Bad_Slug >"$WORK/invalid.log" 2>&1; then
  echo "expected an invalid slug to fail" >&2
  exit 1
fi
if ! grep -q 'lowercase' "$WORK/invalid.log"; then
  echo "invalid slug failed for an unexpected reason" >&2
  cat "$WORK/invalid.log" >&2
  exit 1
fi

echo "Checking that copier update keeps a product Compose file"
"$COPIER" copy "$SRC" "$WORK/update" --defaults --vcs-ref v1.0.0
git -C "$WORK/update" init -q -b main
git_commit "$WORK/update" "Generated"
printf '\n# user-owned stack\n' >> "$WORK/update/docker-compose.yml"
git_commit "$WORK/update" "Customize the stack"
printf '\n# copier-update-marker\n' >> "$SRC/template/scripts/init-unix.sh"
git_commit "$SRC" "Record an engine marker"
git -C "$SRC" tag v1.0.1
"$COPIER" update --skip-answered --defaults "$WORK/update"
grep -q 'user-owned stack' "$WORK/update/docker-compose.yml"
if grep -q '<<<<<<<' "$WORK/update/docker-compose.yml"; then
  echo "update conflicted the user-owned compose file" >&2
  exit 1
fi
grep -q 'copier-update-marker' "$WORK/update/scripts/init-unix.sh"
cmp "$SRC/template/scripts/init-unix.sh" "$WORK/update/scripts/init-unix.sh"

echo "Template checks passed"
