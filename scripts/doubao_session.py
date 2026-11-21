#!/usr/bin/env python3
"""Run Doubao in a Slashie conversation workspace and persist its resume ID.

No credentials are read or stored. SLASHIE_HOME and the provider's own login
remain independent. The caller passes message data only through the environment.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


def main():
    workspace = Path(os.environ.get("SLASHIE_SESSION_DIR") or os.getcwd())
    marker = workspace / "doubao-session.json"
    cli = os.environ.get("DOUBAO_CLI") or shutil.which("doubao") or str(Path.home() / ".local/bin/doubao")
    prompt = os.environ.get("SLASHIE_PROMPT") or os.environ.get("SLASHIE_ARGS", "")
    prompt = prompt.strip()
    if not prompt:
        print("No prompt supplied.", file=sys.stderr)
        return 2
    continuing = os.environ.get("SLASHIE_CONTINUE") == "1"
    command = [cli, "-C", str(workspace)]
    # Only forward an explicitly selected host permission mode. Do not silently
    # disable approval or change provider authorization for ordinary routes.
    permission_mode = os.environ.get("DOUBAO_PERMISSION_MODE")
    if permission_mode:
        if permission_mode not in ("ask_always", "ask_risky", "always_allow"):
            print("Invalid DOUBAO_PERMISSION_MODE.", file=sys.stderr)
            return 2
        command.extend(["--permission-mode", permission_mode])
    if continuing:
        if not marker.exists():
            print("Saved Doubao session is missing; reset this conversation first.", file=sys.stderr)
            return 2
        session = json.loads(marker.read_text())["session_id"]
        command.extend(["--resume", session])
    command.extend(["-p", prompt, "--output-format", "json"])
    # Print mode waits for pipe EOF before starting. Never inherit an open
    # harness input stream; Slashie itself closes command stdin as well.
    result = subprocess.run(command, stdin=subprocess.DEVNULL, capture_output=True, text=True)
    if result.returncode:
        print(result.stderr[-2500:] or result.stdout[-2500:] or "Doubao failed.", file=sys.stderr)
        return result.returncode
    try:
        data = json.loads(result.stdout)
        if data.get("is_error") or data.get("subtype") != "success":
            raise ValueError("Doubao did not return a successful result")
        session_id = str(data["session_id"])
        answer = data.get("result", {})
        text = answer.get("content", "") if isinstance(answer, dict) else str(answer)
        if not session_id or not isinstance(text, str):
            raise ValueError("Missing response or session ID")
    except (ValueError, KeyError, TypeError) as error:
        print(f"Invalid Doubao result: {error}", file=sys.stderr)
        return 2
    temporary = marker.with_suffix(".tmp")
    temporary.write_text(json.dumps({"session_id": session_id}) + "\n")
    temporary.chmod(0o600)
    temporary.replace(marker)
    if os.environ.get("SLASHIE_RECORD_TURNS") == "1":
        evidence = workspace / "doubao-turns.jsonl"
        with evidence.open("a") as stream:
            stream.write(json.dumps({"source_id": os.environ.get("SLASHIE_ID", ""),
                                     "continuing": continuing, "session_id": session_id,
                                     "prompt": prompt, "answer": text}, ensure_ascii=False) + "\n")
        evidence.chmod(0o600)
    print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
