#!/usr/bin/env bash
# Run every example spec, report pass/fail per example.
cd /home/david/work/nxvim
pass=0; fail=0; failed=()
for d in examples/*/; do
  d=${d%/}
  [ -d "$d/test" ] || continue
  if out=$(timeout 180 ./target/debug/bemtvi --test-plugin "$d" 2>&1); then
    n=$(printf '%s' "$out" | grep -oE '[0-9]+ passed' | head -1)
    echo "PASS $d ($n)"; pass=$((pass+1))
  else
    echo "FAIL $d"; printf '%s\n' "$out" | tail -12 | sed 's/^/    /'
    fail=$((fail+1)); failed+=("$d")
  fi
done
echo "--- $pass example suites green, $fail failing ---"
[ ${#failed[@]} -gt 0 ] && printf 'failing: %s\n' "${failed[*]}"
