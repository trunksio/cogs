#!/bin/zsh
# Ingest a sample list into an eval vault, capturing per-file JSON reports
# (.cogs/eval-reports/) and timing on stdout. Resumable; failures reset the
# tree and continue.
#
#   run-pass.sh <vault-dir> <sample-file>   # sample: vault-relative raw paths
set -u
VAULT=$1; SAMPLE=$2
COGS=${COGS:-$(dirname "$0")/../../target/debug/cogs}
export OMLX_API_KEY=${OMLX_API_KEY:-$(python3 -c "import json; print(json.load(open('$HOME/.omlx/settings.json'))['auth']['api_key'])")}
cd "$VAULT"
mkdir -p .cogs/eval-reports
i=0
while IFS= read -r f; do
  i=$((i+1))
  start=$(date +%s)
  if "$COGS" ingest "$f" --json > .cogs/eval-reports/$i.json 2> .cogs/eval-reports/$i.err; then
    echo "[$i] OK $f $(($(date +%s)-start))s"
    git add -A && git commit -qm "ingest: $f"
  else
    echo "[$i] FAIL $f $(($(date +%s)-start))s"
    git checkout -q -- . 2>/dev/null; git clean -qfd wiki 2>/dev/null
  fi
done < "$SAMPLE"
echo "eval run done: $VAULT"
