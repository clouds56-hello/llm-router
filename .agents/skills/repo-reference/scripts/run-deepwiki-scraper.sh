#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "Usage: $0 <owner/repo> <output-dir>" >&2
  exit 1
fi

repo="$1"
output_dir="$2"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_root="$(cd "$script_dir/../../../.." && pwd)"
venv_dir="$workspace_root/.cache/skills/repo-reference/.venv"
requirements_file="$script_dir/requirements.txt"
python_bin="${PYTHON_BIN:-python3}"

if ! command -v "$python_bin" >/dev/null 2>&1; then
  echo "Python executable not found: $python_bin" >&2
  exit 1
fi

if [[ ! -d "$venv_dir" ]]; then
  "$python_bin" -m venv "$venv_dir"
fi

venv_python="$venv_dir/bin/python"

if [[ ! -f "$venv_dir/.deps-ready" || "$requirements_file" -nt "$venv_dir/.deps-ready" ]]; then
  "$venv_python" -m pip install --upgrade pip
  "$venv_python" -m pip install -r "$requirements_file"
  touch "$venv_dir/.deps-ready"
fi

mkdir -p "$output_dir"
exec "$venv_python" "$script_dir/deepwiki-scraper.py" "$repo" "$output_dir"
