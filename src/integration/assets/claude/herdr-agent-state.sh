#!/bin/sh
# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=claude
# HERDR_INTEGRATION_VERSION=7

set -eu

action="${1:-}"
hook_input_file="$(mktemp "${TMPDIR:-/tmp}/herdr-claude-hook.XXXXXX")" || exit 0
trap 'rm -f "$hook_input_file"' EXIT HUP INT TERM
cat >"$hook_input_file" 2>/dev/null || true

case "$action" in
  session|prompt|stop) ;;
  *) exit 0 ;;
esac

[ "${HERDR_ENV:-}" = "1" ] || exit 0
[ -n "${HERDR_SOCKET_PATH:-}" ] || exit 0
[ -n "${HERDR_PANE_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

HERDR_ACTION="$action" HERDR_HOOK_INPUT_FILE="$hook_input_file" python3 - <<'PY'
import json
import os
import random
import socket
import sys
import time

source = "herdr:claude"
action = os.environ.get("HERDR_ACTION", "")
pane_id = os.environ.get("HERDR_PANE_ID")
socket_path = os.environ.get("HERDR_SOCKET_PATH")
hook_input_file = os.environ.get("HERDR_HOOK_INPUT_FILE")

if not pane_id or not socket_path:
    raise SystemExit(0)

hook_input = {}
if hook_input_file:
    try:
        with open(hook_input_file, encoding="utf-8") as handle:
            content = handle.read()
        if content.strip():
            hook_input = json.loads(content)
    except Exception:
        hook_input = {}

hook_event_name = str(hook_input.get("hook_event_name") or "")
is_subagent = bool(hook_input.get("agent_id"))
if hook_event_name == "SubagentStop":
    # SubagentStop is a completion event. Older Herdr integrations mapped it
    # to durable working, but Claude recap/away-summary can emit it after the
    # main turn has already stopped. Never let it revive an idle pane.
    raise SystemExit(0)


def send(request):
    """Fire-and-forget JSON-RPC over the unix socket. Errors are swallowed —
    a hook must never block or fail the parent agent."""
    try:
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.settimeout(0.5)
        client.connect(socket_path)
        client.sendall((json.dumps(request) + "\n").encode())
        try:
            client.recv(4096)
        except Exception:
            pass
        client.close()
    except Exception:
        pass


def now_ids():
    request_id = f"{source}:{int(time.time() * 1000)}:{random.randrange(1_000_000):06d}"
    return request_id, time.time_ns()


# Anything wrapped in `<task-notification>...</task-notification>` (and a few
# sibling system-reminder shapes) is harness-internal noise — not a user
# prompt or assistant message. Filter at the source so it never lands in
# herdr's prompt-history float. Keep the list narrow so prose that happens
# to start with `<` (rare, e.g. JSX) is not dropped.
_SYSTEM_REMINDER_PREFIXES = (
    "<task-notification>",
    "<system-reminder>",
    "<command-name>",
    "<command-message>",
    "<local-command-",
    "<bash-input>",
    "<bash-stdout>",
    "<bash-stderr>",
)


def is_system_reminder(text):
    text = (text or "").lstrip()
    return text.startswith(_SYSTEM_REMINDER_PREFIXES)


if action == "prompt":
    prompt = hook_input.get("prompt")
    if not isinstance(prompt, str) or not prompt.strip():
        raise SystemExit(0)
    if is_system_reminder(prompt):
        # The harness sends task-completion / command-injection markers
        # through the same UserPromptSubmit pipe as real prompts. Drop
        # them: they are not the user's words.
        raise SystemExit(0)
    request_id, report_seq = now_ids()
    send({
        "id": request_id,
        "method": "pane.report_prompt",
        "params": {
            "pane_id": pane_id,
            "source": source,
            "agent": "claude",
            "seq": report_seq,
            # Server re-sanitizes; cap bounds the wire payload.
            "prompt": prompt[:16384],
        },
    })
    raise SystemExit(0)


if action == "session":
    session_id = hook_input.get("session_id")
    agent_session_id = session_id if isinstance(session_id, str) and session_id else None
    if not agent_session_id:
        raise SystemExit(0)
    request_id, report_seq = now_ids()
    send({
        "id": request_id,
        "method": "pane.report_agent_session",
        "params": {
            "pane_id": pane_id,
            "source": source,
            "agent": "claude",
            "seq": report_seq,
            "agent_session_id": agent_session_id,
        },
    })
    raise SystemExit(0)


# action == "stop": scrape transcript_path for the last assistant message,
# POST it as a reply, and lift the `※ recap:` sentinel line (if present) as
# the recap. If no sentinel is present, return decision:block so the agent
# gets one more turn to emit one (self-healing nudge, never user-facing).

transcript_path = hook_input.get("transcript_path")
last_assistant_text = ""
if isinstance(transcript_path, str) and transcript_path:
    try:
        with open(transcript_path, encoding="utf-8", errors="replace") as handle:
            # Transcripts are JSONL; the last assistant message is the last
            # line whose role is "assistant" with a text content block.
            for raw_line in reversed(list(handle)):
                raw_line = raw_line.strip()
                if not raw_line:
                    continue
                try:
                    obj = json.loads(raw_line)
                except Exception:
                    continue
                # Claude transcript shapes vary by version; check both the
                # top-level role and the nested message.role field.
                role = obj.get("role") or obj.get("type")
                message = obj.get("message") if isinstance(obj.get("message"), dict) else None
                if message and not role:
                    role = message.get("role")
                if role != "assistant":
                    continue
                # Extract text from a content array (list of blocks with
                # type=text) or a plain string content field.
                content = (message or obj).get("content")
                texts = []
                if isinstance(content, str):
                    texts.append(content)
                elif isinstance(content, list):
                    for block in content:
                        if not isinstance(block, dict):
                            continue
                        if block.get("type") == "text" and isinstance(block.get("text"), str):
                            texts.append(block["text"])
                if texts:
                    last_assistant_text = "\n".join(t for t in texts if t).strip()
                    if last_assistant_text:
                        break
    except Exception:
        last_assistant_text = ""

# Optional reply: skip if we have nothing or if the message is just a harness
# system-reminder leaking through. Cap at 4KB on the wire so a long thinking
# reply doesn't crowd the float; recap is the readable summary.
if last_assistant_text and not is_system_reminder(last_assistant_text):
    request_id, report_seq = now_ids()
    send({
        "id": request_id,
        "method": "pane.report_reply",
        "params": {
            "pane_id": pane_id,
            "source": source,
            "agent": "claude",
            "seq": report_seq,
            "reply": last_assistant_text[:4096],
        },
    })

# Recap extraction: lift the `※ recap:` sentinel line (or any line starting
# with the sentinel char) verbatim. If absent, fall through to the
# decision:block nudge below.
recap_match = None
for line in last_assistant_text.splitlines():
    stripped = line.strip()
    if stripped.startswith("※"):
        recap_match = stripped
        break

if recap_match:
    request_id, report_seq = now_ids()
    send({
        "id": request_id,
        "method": "pane.report_recap",
        "params": {
            "pane_id": pane_id,
            "source": source,
            "agent": "claude",
            "seq": report_seq,
            "recap": recap_match[:4096],
        },
    })
    raise SystemExit(0)

# No sentinel: nudge the agent for one more turn instead of stopping. Claude
# Code's Stop hook reads decision/reason from stdout; `decision: "block"`
# unstops with the reason as additional context. Self-healing without user
# friction. Skip the nudge when we never saw any assistant text (subagent /
# empty transcript) so we don't loop on nothing.
if last_assistant_text and not is_subagent:
    nudge = {
        "decision": "block",
        "reason": (
            "End your turn with a single sentinel line: `※ recap: "
            "<current state>. Next: <one concrete step>.` Then stop."
        ),
    }
    try:
        sys.stdout.write(json.dumps(nudge))
        sys.stdout.flush()
    except Exception:
        pass

raise SystemExit(0)
PY
