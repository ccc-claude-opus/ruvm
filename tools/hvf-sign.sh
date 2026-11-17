#!/bin/sh
# Sign ruvm (ad hoc) with the Hypervisor.framework entitlement so that -accel hvf works.
# Run after every build: tools/hvf-sign.sh [binary]   (default target/release/ruvm)
set -e
cd "$(dirname "$0")/.."
bin=${1:-target/release/ruvm}
codesign --sign - --force --entitlements tools/hvf.entitlements "$bin"
echo "signed $bin for -accel hvf"
