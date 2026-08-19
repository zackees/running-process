#!/usr/bin/env bash
set -euo pipefail

# GitHub's amd64 Ubuntu runners prefer azure.archive.ubuntu.com through an
# apt mirrorlist. That endpoint can accept the connection and then stop
# serving indexes for the lifetime of a CI job. Use the canonical archive
# directly, with the ports archive for native arm64 runners.
. /etc/os-release
: "${VERSION_CODENAME:?missing Ubuntu VERSION_CODENAME}"

case "$(dpkg --print-architecture)" in
    amd64) archive="https://archive.ubuntu.com/ubuntu" ;;
    arm64) archive="https://ports.ubuntu.com/ubuntu-ports" ;;
    *)
        echo "unsupported Ubuntu architecture: $(dpkg --print-architecture)" >&2
        exit 2
        ;;
esac

sources="$(mktemp --suffix=.sources)"
trap 'rm -f "$sources"' EXIT
cat >"$sources" <<EOF
Types: deb
URIs: $archive
Suites: $VERSION_CODENAME $VERSION_CODENAME-updates $VERSION_CODENAME-security
Components: main universe
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
EOF

apt_options=(
    -o "Dir::Etc::sourcelist=$sources"
    -o "Dir::Etc::sourceparts=-"
    -o "Acquire::Retries=5"
    -o "Acquire::http::Timeout=30"
    -o "Acquire::https::Timeout=30"
)

sudo timeout --kill-after=15s 300s apt-get "${apt_options[@]}" update
sudo env DEBIAN_FRONTEND=noninteractive timeout --kill-after=15s 300s \
    apt-get "${apt_options[@]}" install -y --no-install-recommends musl-tools
