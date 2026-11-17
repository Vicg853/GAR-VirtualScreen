#!/usr/bin/env bash
# Kept for existing habits: builds dist/AirBridge Host.app. See bundle.sh.
exec "$(dirname "$0")/bundle.sh" "$@"
