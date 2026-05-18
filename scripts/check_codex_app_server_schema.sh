#!/usr/bin/env bash
set -euo pipefail

out_dir="${1:-}"
cleanup=0
if [[ -z "$out_dir" ]]; then
  out_dir="$(mktemp -d)"
  cleanup=1
fi

if ! command -v codex >/dev/null 2>&1; then
  echo "codex CLI is not installed; cannot check app-server schema drift" >&2
  exit 127
fi

mkdir -p "$out_dir"
codex app-server generate-json-schema --experimental --out "$out_dir"

required_patterns=(
  '"thread/start"'
  '"thread/resume"'
  '"turn/start"'
  '"thread/read"'
  '"thread/started"'
  '"turn/started"'
  '"CommandExecutionRequestApprovalParams"'
  '"FileChangeRequestApprovalParams"'
)

for pattern in "${required_patterns[@]}"; do
  if ! rg -q --fixed-strings "$pattern" "$out_dir"; then
    echo "required app-server schema pattern missing: $pattern" >&2
    echo "schema output directory: $out_dir" >&2
    exit 1
  fi
done

schema_count="$(find "$out_dir" -type f \( -name '*.json' -o -name '*.schema.json' \) | wc -l)"
if [[ "${schema_count//[[:space:]]/}" == "0" ]]; then
  echo "codex generated no JSON schema files in $out_dir" >&2
  exit 1
fi

echo "codex app-server schema check passed: $schema_count schema file(s) in $out_dir"

if [[ "$cleanup" == "1" ]]; then
  rm -rf "$out_dir"
fi
