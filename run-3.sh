#!/usr/bin/env bash
# Rebuilds from the latest sources, then opens a three-way merge of the sample files.
# The result goes to a fresh temporary file each time, so the samples never change.
#
# What the samples contain:
#   merged automatically: the header comment (local), quantity as f64 (same change on
#   both sides), the new discounted() function and the euro sign (distant);
#   conflicts: tax(), the new field in Line (two insertions at the same place), the
#   labels (two adjacent lines: "Combiner" applies both) and round() (deleted locally,
#   commented on the distant side).
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"
nice -n 15 cargo build --profile fast -p biomerge-app
samples=samples/merge
result="$(mktemp -d)/facture.rs"
echo "Résultat : $result"
exec target/fast/biomerge --merge \
  -L "facture.rs (base)" -L "facture.rs (local)" -L "facture.rs (distant)" \
  "$samples/base.rs" "$samples/local.rs" "$samples/distant.rs" "$result"
