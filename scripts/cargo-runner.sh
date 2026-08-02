#!/bin/sh

set -eu

if [ "$#" -lt 1 ]; then
    echo "usage: cargo-runner.sh <firmware-elf>" >&2
    exit 2
fi

firmware=$1
shift

case "$firmware" in
    */thumbv8m.main-none-eabihf/*)
        if ! command -v picotool >/dev/null 2>&1; then
            echo "picotool is required to flash RP2350 firmware" >&2
            exit 127
        fi

        uf2=$(mktemp "${TMPDIR:-/tmp}/pico-io-bridge-rp2350.XXXXXX")
        trap 'rm -f "$uf2"' EXIT HUP INT TERM

        picotool uf2 convert \
            "$firmware" -t elf \
            "$uf2" -t uf2 \
            --family rp2350-arm-s \
            --abs-block

        if [ "${PICO_IO_RUNNER_DRY_RUN:-0}" = "1" ]; then
            echo "dry run: picotool load -u -v -x $uf2 -t uf2"
            exit 0
        fi

        picotool load -u -v -x "$uf2" -t uf2
        ;;
    *)
        if ! command -v elf2uf2-rs >/dev/null 2>&1; then
            echo "elf2uf2-rs is required to flash RP2040 firmware" >&2
            exit 127
        fi

        if [ "${PICO_IO_RUNNER_DRY_RUN:-0}" = "1" ]; then
            echo "dry run: elf2uf2-rs --deploy --serial --verbose $firmware $*"
            exit 0
        fi

        exec elf2uf2-rs --deploy --serial --verbose "$firmware" "$@"
        ;;
esac
