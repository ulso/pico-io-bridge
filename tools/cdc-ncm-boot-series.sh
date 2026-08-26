#!/bin/bash
# Boot series for the CDC-NCM link.
#
# Each cycle drops the USB pullup and resets, which the host sees as a real
# re-attachment. The previous version of this script reset again as soon as the
# HTTP server answered, which is long before the board has finished starting
# up -- mDNS, DHCP and the host-silence probe all come after. Repeatedly
# interrupting a board mid-startup is a regime the firmware never meets in
# life, and it eventually left one with its USB pullup down and no way back
# except the BOOTSEL button. So: wait for the board to say it is ready, and
# only then reset it.
#
# Two times are recorded per cycle. Time-to-link is when HTTP first answers.
# Time-to-ready is when startup completes. The discriminator for the Apple bug
# is the reset cause: a cycle the firmware had to heal itself out of reports
# CDC_NCM_HOST_SILENCE_RECOVERY or a link-watchdog cause, and takes over ten
# seconds; a clean attachment reports our own reset and takes a few.

IP=${IP:-10.120.39.1}
N=${N:-30}
LIMIT=${LIMIT:-45}          # seconds before a cycle counts as dead
READY_LIMIT=${READY_LIMIT:-30}
OUT=${OUT:-/dev/stdout}

now() { date +%s; }

printf 'cycle\tlink_s\tready_s\tresetCause\n' > "$OUT"
clean=0; healed=0; dead=0

for i in $(seq 1 "$N"); do
    curl -s -m 3 "http://$IP/api/link-measure/reset" >/dev/null 2>&1
    sleep 1                                  # let the pullup actually drop

    start=$(now); up=0
    while [ $(( $(now) - start )) -lt "$LIMIT" ]; do
        curl -s -m 1 "http://$IP/api/status" -o /dev/null 2>/dev/null && { up=1; break; }
        sleep 0.25
    done
    link=$(( $(now) - start ))

    if [ "$up" = 0 ]; then
        printf '%d\t%d\t-\tNO-LINK\n' "$i" "$link" >> "$OUT"
        dead=$((dead+1))
        continue
    fi

    # Wait for startup to finish before touching the board again.
    rstart=$(now); ready=0
    while [ $(( $(now) - rstart )) -lt "$READY_LIMIT" ]; do
        [ "$(curl -s -m 2 "http://$IP/api/link-measure/ready" 2>/dev/null | tr -d '[:space:]')" = "1" ] \
            && { ready=1; break; }
        sleep 0.5
    done
    rdy=$(( $(now) - rstart ))

    cause=$(curl -s -m 3 "http://$IP/api/usb-host/status" 2>/dev/null \
            | sed -n 's/.*"resetCause":"\([^"]*\)".*/\1/p')
    [ -z "$cause" ] && cause="?"

    case "$cause" in
        *HOST_SILENCE*|*LINK_WATCHDOG*) healed=$((healed+1)) ;;
        *) clean=$((clean+1)) ;;
    esac

    [ "$ready" = 0 ] && rdy="timeout"
    printf '%d\t%d\t%s\t%s\n' "$i" "$link" "$rdy" "$cause" >> "$OUT"
    sleep 2                                  # settle before the next cycle
done

printf '\nclean:  %d\nhealed: %d\nno link:%d\nof %d\n' "$clean" "$healed" "$dead" "$N" >> "$OUT"
