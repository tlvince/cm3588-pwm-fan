#!/usr/bin/env bash
set -euo pipefail

# Acquire sudo before starting so it doesn't prompt midway through.
sudo -v

H="$(
  for d in /sys/class/hwmon/hwmon*; do
    if [[ "$(cat "$d/name" 2>/dev/null || true)" == "pwmfan" ]]; then
      echo "$d"
      break
    fi
  done
)"

if [[ -z "${H:-}" ]]; then
  echo "ERROR: pwmfan hwmon device not found." >&2
  exit 1
fi

PWM="$H/pwm1"
RPM="$H/fan1_input"

if [[ ! -e "$PWM" || ! -e "$RPM" ]]; then
  echo "ERROR: pwm1 or fan1_input missing under $H." >&2
  exit 1
fi

ORIGINAL_PWM="$(cat "$PWM")"

restore() {
  echo
  echo "Restoring PWM to $ORIGINAL_PWM..."
  printf '%s\n' "$ORIGINAL_PWM" | sudo tee "$PWM" >/dev/null || true
}
trap restore EXIT INT TERM

set_pwm() {
  local value="$1"

  printf '%s\n' "$value" | sudo tee "$PWM" >/dev/null
  sleep 0.2

  local actual
  actual="$(cat "$PWM")"

  if [[ "$actual" != "$value" ]]; then
    echo "ERROR: requested PWM $value but pwm1 reads $actual." >&2
    echo "The thermal framework may be overriding manual control." >&2
    exit 1
  fi
}

echo "pwmfan:       $H"
echo "original PWM: $ORIGINAL_PWM"
echo "current RPM:  $(cat "$RPM")"
echo

###############################################################################
# 1. Find minimum reliable cold-start PWM
###############################################################################

START_CSV="fan-start-test.csv"

echo "pwm,attempt,rpm,started" > "$START_CSV"

echo "=== Cold-start test: PWM 20..30, 5 attempts each ==="
echo

for p in $(seq 20 30); do
  successes=0

  for attempt in $(seq 1 5); do
    # PWM 19 is known to stop this particular fan.
    set_pwm 19
    sleep 4

    stopped_rpm="$(cat "$RPM")"

    # Wait a little longer if hwmon hasn't yet reported zero.
    if (( stopped_rpm > 0 )); then
      sleep 3
    fi

    set_pwm "$p"
    sleep 6

    rpm="$(cat "$RPM")"

    if (( rpm > 0 )); then
      started=1
      ((++successes))
    else
      started=0
    fi

    printf "PWM %2d  attempt %d/5  -> %4d RPM  %s\n" \
      "$p" "$attempt" "$rpm" \
      "$([[ "$started" == 1 ]] && echo STARTED || echo FAILED)"

    echo "$p,$attempt,$rpm,$started" >> "$START_CSV"
  done

  echo "PWM $p: $successes/5 successful starts"
  echo
done

###############################################################################
# Determine lowest value with 5/5 starts
###############################################################################

MIN_RELIABLE="$(
  awk -F, '
    NR > 1 {
      count[$1]++
      success[$1] += $4
    }
    END {
      for (p in count)
        if (count[p] == 5 && success[p] == 5)
          print p
    }
  ' "$START_CSV" | sort -n | head -1
)"

###############################################################################
# 2. Measure PWM/RPM curve
###############################################################################

CURVE_CSV="fan-curve.csv"
echo "pwm,duty_percent,average_rpm,min_rpm,max_rpm" > "$CURVE_CSV"

POINTS=(
  20 21 22 23 24 25 27 30 35 40
  50 60 75 100 125 150 175 200 225 255
)

echo
echo "=== Measuring PWM/RPM curve ==="
echo

for p in "${POINTS[@]}"; do
  # Always spin up first so this part measures running RPM,
  # independently of the cold-start threshold.
  set_pwm 255
  sleep 2

  set_pwm "$p"
  sleep 6

  stats="$(
    for _ in $(seq 1 10); do
      cat "$RPM"
      sleep 1
    done | awk '
      NR == 1 {
        min=$1
        max=$1
      }
      {
        sum += $1
        if ($1 < min) min=$1
        if ($1 > max) max=$1
      }
      END {
        printf "%.1f,%d,%d", sum/NR, min, max
      }
    '
  )"

  duty="$(awk -v p="$p" 'BEGIN { printf "%.1f", p / 255 * 100 }')"

  echo "$p,$duty,$stats" >> "$CURVE_CSV"

  IFS=, read -r avg min max <<< "$stats"

  printf "PWM %3d  %5.1f%%  avg=%7.1f RPM  min=%4d  max=%4d\n" \
    "$p" "$duty" "$avg" "$min" "$max"
done

###############################################################################
# Results
###############################################################################

echo
echo "============================================================"
echo "RESULTS"
echo "============================================================"

if [[ -n "$MIN_RELIABLE" ]]; then
  echo "Lowest PWM with 5/5 successful cold starts: $MIN_RELIABLE"

  # Give a small safety margin if possible.
  RECOMMENDED=$(( MIN_RELIABLE + 2 ))

  echo "Suggested unattended minimum with small margin: $RECOMMENDED"
else
  echo "No PWM value from 20 through 30 achieved 5/5 starts."
  echo "Extend the startup test range."
fi

echo
echo "Startup results: $START_CSV"
echo "Fan curve:       $CURVE_CSV"
echo
echo "Current fan will now be restored to PWM $ORIGINAL_PWM."
