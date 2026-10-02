#!/usr/bin/env python3
"""traj_extract.py -- turn a recorded AgentSysPerf fixture into a guest trajectory.

The fixture holds the LLM's *responses*, one per agent turn (schema in
fixture.py / AgentSysPerf's src/replay/fixture.py). The agent loop turned each
response into exactly one shell command, either as an OpenAI tool call or, when
the model has no tool parser (our vLLM), in ReACT text:

    Thought: ...
    Action: shell
    Action Input: {"command": "python3 /tmp/generate_gates.py"}

This pulls that command back out per turn and writes <name>.traj.json, which
agent_traj.py executes inside the guest one turn per agent step. Turns with no
action (the model's final answer, or a malformed reply the loop rejected) are
kept with command=null so turn numbering matches the recording.

usage: traj_extract.py fixture.jsonl -o name.traj.json [--task ID] [--workdir /app]
"""
import argparse
import json
import re
import sys
from pathlib import Path

# ReACT: "Action: <tool>" then "Action Input: <json or text>", input may span
# lines and may be fenced. Non-greedy to the first balanced-looking close.
RE_ACTION = re.compile(
    r"Action:\s*(?P<tool>[\w\-]+)\s*\n\s*Action Input:\s*(?P<input>.+?)(?:\n\s*(?:Observation|Thought|Final Answer):|\Z)",
    re.S,
)
RE_FENCE = re.compile(r"^```[\w-]*\s*|\s*```$", re.S)
SHELL_TOOLS = {"shell", "bash", "sh", "run", "run_command", "execute", "terminal", "exec"}


def command_from_react(content: str):
    m = RE_ACTION.search(content or "")
    if not m:
        return None, None
    tool = m.group("tool").strip().lower()
    raw = RE_FENCE.sub("", m.group("input").strip())
    cmd = None
    try:
        obj = json.loads(raw)
        if isinstance(obj, dict):
            cmd = obj.get("command") or obj.get("cmd") or obj.get("input")
        elif isinstance(obj, str):
            cmd = obj
    except json.JSONDecodeError:
        # Mirror the agent loop, not the model: when Action Input is not valid
        # JSON (typically a generation that hit max_tokens mid-command, e.g. a
        # 4096-token chain of echo >> gates.txt), the loop executes NOTHING and
        # sends the model a format correction. Running the raw text here would
        # replay something the agent never did. Keep the turn as a no-op.
        return "parse_fail", None
    return tool, cmd


def command_from_tool_calls(tool_calls):
    for tc in tool_calls or []:
        fn = (tc.get("function") or {})
        name = (fn.get("name") or "").lower()
        try:
            args = json.loads(fn.get("arguments") or "{}")
        except json.JSONDecodeError:
            args = {}
        cmd = args.get("command") or args.get("cmd")
        if cmd:
            return name, cmd
    return None, None


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("fixture", type=Path)
    p.add_argument("-o", "--out", type=Path, required=True)
    p.add_argument("--task", default="", help="Harbor task id, for the record")
    p.add_argument("--image", default="", help="Docker image the agent ran in")
    p.add_argument("--workdir", default="/app", help="cwd the agent's shell had")
    p.add_argument("--trial", default="", help="pick one trial_key if the fixture holds several")
    a = p.parse_args(argv)

    rows = [json.loads(l) for l in a.fixture.read_text().splitlines() if l.strip()]
    trials = sorted({r["trial_key"] for r in rows})
    if not rows:
        print("empty fixture", file=sys.stderr); return 2
    key = a.trial or trials[0]
    if len(trials) > 1 and not a.trial:
        print(f"note: {len(trials)} trials in fixture, using {key}", file=sys.stderr)
    rows = sorted((r for r in rows if r["trial_key"] == key), key=lambda r: r["turn"])

    turns, n_cmd, n_tool, n_react, n_fail = [], 0, 0, 0, 0
    for r in rows:
        msg = r["response"]["choices"][0]["message"]
        tool, cmd = command_from_tool_calls(msg.get("tool_calls"))
        how = "tool_call" if cmd else None
        if not cmd:
            tool, cmd = command_from_react(msg.get("content") or "")
            how = "react" if cmd else None
        if cmd:
            n_cmd += 1
            n_tool += how == "tool_call"
            n_react += how == "react"
        elif tool == "parse_fail":
            n_fail += 1
        thought = (msg.get("content") or "").strip().split("\n", 1)[0][:120]
        usage = r["response"].get("usage") or {}
        turns.append({
            "turn": r["turn"],
            "tool": tool,
            "command": cmd,
            "how": how,
            "thought": thought,
            "latency_ms": r.get("latency_ms"),
            "prompt_tokens": usage.get("prompt_tokens"),
            "completion_tokens": usage.get("completion_tokens"),
        })

    out = {
        "task": a.task,
        "image": a.image,
        "workdir": a.workdir,
        "trial_key": key,
        "source_fixture": str(a.fixture),
        "n_turns": len(turns),
        "n_commands": n_cmd,
        "turns": turns,
    }
    a.out.write_text(json.dumps(out, indent=1) + "\n")
    print(f"{a.out}: trial {key}, {len(turns)} turns, {n_cmd} commands "
          f"({n_react} ReACT, {n_tool} tool_call), {len(turns) - n_cmd} without an action"
          f"{f' ({n_fail} unparseable -> no-op, as the loop did)' if n_fail else ''}")
    for t in turns:
        c = (t["command"] or "-").replace("\n", "⏎")
        print(f"  t{t['turn']:02d} {t['tool'] or '-':6} {c[:100]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
