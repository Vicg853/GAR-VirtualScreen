#!/usr/bin/env bash
# Kept for existing habits: builds the viewer bundle and installs it for the
# current user. See bundle.sh.
exec "$(dirname "$0")/bundle.sh" --install "$@"
