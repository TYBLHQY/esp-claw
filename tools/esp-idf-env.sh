#!/usr/bin/env bash

# Source this file before using idf.py in this checkout:
#   source tools/esp-idf-env.sh

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    echo "Please source this file: source $0" >&2
    exit 1
fi

ESPCLAW_REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ESPCLAW_IDF_PATH="${ESPCLAW_REPO_ROOT}/../.toolchains/esp-idf-v5.5.4"

if [[ ! -f "${ESPCLAW_IDF_PATH}/export.sh" ]]; then
    echo "ESP-IDF not found at ${ESPCLAW_IDF_PATH}" >&2
    return 1
fi

source "${ESPCLAW_IDF_PATH}/export.sh"
export ESPCLAW_REPO_ROOT
export ESPCLAW_IDF_PATH

echo "ESP-Claw ESP-IDF environment ready: ${ESPCLAW_IDF_PATH}"
