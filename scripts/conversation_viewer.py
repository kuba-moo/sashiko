#!/usr/bin/env python3
"""TUI viewer for sashiko conversation dumps.

Usage:
    python3 scripts/conversation_viewer.py /srv/nipa-sashiko/sashiko-dump/
"""
from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from typing import Any

from textual import on
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical
from textual.reactive import reactive
from textual.widgets import (
    Footer,
    Header,
    Static,
    TextArea,
    Tree,
)


STAGE_NAMES = {
    "s0": "Phase 0: Patch Classification",
    "sp": "Planning",
    "s1": "Stage 1: Goal Analysis",
    "s2": "Stage 2: Implementation Verification",
    "s3": "Stage 3: Execution Flow",
    "s4": "Stage 4: Resource Management",
    "s5": "Stage 5: Locking",
    "s6": "Stage 6: Security",
    "s7": "Stage 7: Hardware",
    "s8": "Stage 8: Verification",
    "s9": "Stage 9: Report",
}


def parse_filename(name: str) -> tuple[str, int, str]:
    """Parse 's1_003_req.json' -> ('s1', 3, 'req')"""
    parts = name.replace(".json", "").split("_")
    return parts[0], int(parts[1]), parts[2]


def load_dump_dir(dump_dir: Path) -> dict[str, dict[int, dict[str, Any]]]:
    """Load all dump files into {stage: {turn: {req: ..., resp: ...}}}"""
    data: dict[str, dict[int, dict[str, Any]]] = {}
    for f in sorted(dump_dir.iterdir()):
        if not f.suffix == ".json":
            continue
        try:
            stage, turn, kind = parse_filename(f.name)
        except (ValueError, IndexError):
            continue
        if stage not in data:
            data[stage] = {}
        if turn not in data[stage]:
            data[stage][turn] = {}
        with open(f) as fh:
            data[stage][turn][kind] = json.load(fh)
    return data


def format_tool_call(tc: dict) -> str:
    """Format a tool call for display."""
    name = tc.get("function_name", tc.get("name", "unknown"))
    args = tc.get("arguments", "{}")
    if isinstance(args, str):
        try:
            args = json.loads(args)
        except json.JSONDecodeError:
            pass
    tc_id = tc.get("id", "")
    lines = [f"┌─ Tool Call: {name}"]
    if tc_id:
        lines.append(f"│  ID: {tc_id}")
    if isinstance(args, dict):
        for k, v in args.items():
            val_str = json.dumps(v) if not isinstance(v, str) else v
            if len(val_str) > 120:
                val_str = val_str[:117] + "..."
            lines.append(f"│  {k}: {val_str}")
    else:
        lines.append(f"│  args: {args}")
    lines.append("└─")
    return "\n".join(lines)


def format_usage(usage: dict) -> str:
    """Format token usage."""
    parts = []
    if "prompt_tokens" in usage:
        parts.append(f"Input: {usage['prompt_tokens']:,}")
    if "cached_tokens" in usage and usage["cached_tokens"]:
        parts.append(f"Cached: {usage['cached_tokens']:,}")
    if "completion_tokens" in usage:
        parts.append(f"Output: {usage['completion_tokens']:,}")
    if "total_tokens" in usage:
        parts.append(f"Total: {usage['total_tokens']:,}")
    return " │ ".join(parts)


def format_message(msg: dict) -> str:
    """Format a single message for display."""
    role = msg.get("role", "unknown")
    content = msg.get("content", "")
    lines = []

    role_display = role.upper()
    lines.append(f"{'═' * 60}")
    lines.append(f"  {role_display}")
    lines.append(f"{'─' * 60}")

    if isinstance(content, str) and content:
        lines.append(content)
    elif isinstance(content, list):
        for part in content:
            if isinstance(part, dict):
                if part.get("type") == "text":
                    lines.append(part.get("text", ""))
                elif part.get("type") == "tool_use":
                    lines.append(format_tool_call(part))
                elif part.get("type") == "tool_result":
                    lines.append(f"[Tool Result: {part.get('tool_use_id', '')}]")
                    lines.append(str(part.get("content", ""))[:2000])

    if "tool_calls" in msg and msg["tool_calls"]:
        lines.append("")
        for tc in msg["tool_calls"]:
            lines.append(format_tool_call(tc))

    if "tool_call_id" in msg:
        lines.append(f"[Response to tool_call: {msg['tool_call_id']}]")
        if isinstance(content, str):
            lines.append(content)

    return "\n".join(lines)


def build_turn_text(
    turn_data: dict[str, Any],
    stage: str,
    turn_num: int,
    full_data: dict,
    show_full_system: bool = False,
    truncate_tool_results: bool = True,
) -> str:
    """Build the full text for a turn (request + response)."""
    lines = []

    req = turn_data.get("req")
    resp = turn_data.get("resp")

    lines.append(f"{'█' * 70}")
    stage_name = STAGE_NAMES.get(stage, stage)
    lines.append(f"  {stage_name} — Turn {turn_num}")
    lines.append(f"{'█' * 70}")
    lines.append("")

    if req:
        if isinstance(req, dict) and "system" in req:
            # Full request (turn 1)
            lines.append(f"┌{'─' * 68}┐")
            lines.append(f"│ SYSTEM PROMPT ({len(req['system']):,} chars)")
            lines.append(f"└{'─' * 68}┘")
            sys_text = req["system"]
            if not show_full_system and len(sys_text) > 5000:
                lines.append(sys_text[:2500])
                lines.append(f"\n[... {len(sys_text) - 5000:,} chars omitted — press 'f' for full ...]\n")
                lines.append(sys_text[-2500:])
            else:
                lines.append(sys_text)
            lines.append("")

            # Tools
            if "tools" in req and req["tools"]:
                lines.append(f"┌{'─' * 68}┐")
                lines.append(f"│ TOOLS ({len(req['tools'])} available)")
                lines.append(f"└{'─' * 68}┘")
                for tool in req["tools"]:
                    name = tool.get("name", "?")
                    desc = tool.get("description", "")[:80]
                    lines.append(f"  • {name}: {desc}")
                lines.append("")

            # Temperature and context
            if "temperature" in req:
                lines.append(f"Temperature: {req['temperature']}")
            if "context_tag" in req:
                lines.append(f"Context tag: {req['context_tag']}")
            if "response_format" in req:
                lines.append(f"Response format: {json.dumps(req['response_format'])}")
            lines.append("")

            # Messages
            if "messages" in req:
                lines.append(f"┌{'─' * 68}┐")
                lines.append(f"│ MESSAGES ({len(req['messages'])})")
                lines.append(f"└{'─' * 68}┘")
                for msg in req["messages"]:
                    lines.append(format_message(msg))
                lines.append("")

        elif isinstance(req, list):
            # Incremental request (turn 2+)
            lines.append(f"┌{'─' * 68}┐")
            lines.append(f"│ INCREMENTAL MESSAGES ({len(req)} new)")
            lines.append(f"└{'─' * 68}┘")
            for msg in req:
                lines.append(format_message(msg))
            lines.append("")

    if resp:
        lines.append(f"┌{'─' * 68}┐")
        lines.append(f"│ RESPONSE")
        lines.append(f"└{'─' * 68}┘")

        if "content" in resp and resp["content"]:
            lines.append("")
            lines.append("Text:")
            lines.append(resp["content"])

        if "tool_calls" in resp and resp["tool_calls"]:
            lines.append("")
            lines.append(f"Tool Calls ({len(resp['tool_calls'])}):")
            for tc in resp["tool_calls"]:
                lines.append(format_tool_call(tc))

        if "usage" in resp:
            lines.append("")
            lines.append(f"Tokens: {format_usage(resp['usage'])}")

    lines.append("")
    return "\n".join(lines)


class StageTree(Tree):
    """Tree widget showing stages and turns."""
    pass


class ConversationViewer(App):
    """TUI for viewing sashiko conversation dumps."""

    CSS = """
    #main {
        layout: horizontal;
    }
    #sidebar {
        width: 35;
        border-right: solid $primary;
    }
    #sidebar Tree {
        width: 100%;
    }
    #content {
        width: 1fr;
    }
    #content-area {
        width: 100%;
        height: 100%;
    }
    #status-bar {
        height: 1;
        background: $surface;
        color: $text-muted;
        padding: 0 1;
    }
    #stage-summary {
        height: 3;
        background: $surface;
        border-bottom: solid $primary;
        padding: 0 1;
    }
    """

    BINDINGS = [
        Binding("q", "quit", "Quit"),
        Binding("j", "next_turn", "Next Turn"),
        Binding("k", "prev_turn", "Prev Turn"),
        Binding("n", "next_stage", "Next Stage"),
        Binding("p", "prev_stage", "Prev Stage"),
        Binding("t", "toggle_truncate", "Toggle Truncate"),
        Binding("f", "full_system", "Full System Prompt"),
    ]

    show_full_system = reactive(False)
    truncate_tool_results = reactive(True)

    def __init__(self, dump_dir: Path):
        super().__init__()
        self.dump_dir = dump_dir
        self.data = load_dump_dir(dump_dir)
        self.stages_sorted = sorted(self.data.keys(), key=self._stage_sort_key)
        self.current_stage: str | None = None
        self.current_turn: int | None = None

    @staticmethod
    def _stage_sort_key(s: str) -> tuple[int, str]:
        if s == "s0":
            return (0, "")
        if s == "sp":
            return (1, "")
        try:
            return (2, s)
        except ValueError:
            return (3, s)

    def compose(self) -> ComposeResult:
        yield Header()
        with Horizontal(id="main"):
            with Vertical(id="sidebar"):
                tree: Tree[dict] = Tree("Conversation", id="stage-tree")
                tree.root.expand()
                for stage in self.stages_sorted:
                    stage_name = STAGE_NAMES.get(stage, stage)
                    turns = sorted(self.data[stage].keys())
                    # Compute total tokens for stage
                    total_in = sum(
                        self.data[stage][t].get("resp", {}).get("usage", {}).get("prompt_tokens", 0)
                        for t in turns
                    )
                    total_out = sum(
                        self.data[stage][t].get("resp", {}).get("usage", {}).get("completion_tokens", 0)
                        for t in turns
                    )
                    label = f"{stage_name} [{len(turns)}t]"
                    stage_node = tree.root.add(label, data={"stage": stage, "turn": None})
                    for t in turns:
                        resp = self.data[stage][t].get("resp", {})
                        usage = resp.get("usage", {})
                        tok_out = usage.get("completion_tokens", 0)
                        tool_calls = resp.get("tool_calls", [])
                        tc_names = [tc.get("function_name", "?") for tc in tool_calls] if tool_calls else []
                        turn_label = f"Turn {t}"
                        if tc_names:
                            turn_label += f": {', '.join(tc_names[:3])}"
                            if len(tc_names) > 3:
                                turn_label += f" +{len(tc_names)-3}"
                        elif resp.get("content"):
                            preview = resp["content"][:40].replace("\n", " ")
                            turn_label += f": {preview}..."
                        stage_node.add_leaf(turn_label, data={"stage": stage, "turn": t})
                yield tree
            with Vertical(id="content"):
                yield Static("", id="stage-summary")
                yield TextArea(id="content-area", read_only=True, show_line_numbers=True)
        yield Static("", id="status-bar")
        yield Footer()

    def on_mount(self) -> None:
        self.title = f"Sashiko Conversation Viewer — {self.dump_dir}"
        if self.stages_sorted:
            self.current_stage = self.stages_sorted[0]
            self.current_turn = min(self.data[self.current_stage].keys())
            self._update_display()

    @on(Tree.NodeSelected)
    def on_tree_node_selected(self, event: Tree.NodeSelected) -> None:
        if event.node.data is None:
            return
        stage = event.node.data.get("stage")
        turn = event.node.data.get("turn")
        if stage:
            self.current_stage = stage
            if turn:
                self.current_turn = turn
            else:
                self.current_turn = min(self.data[stage].keys())
            self._update_display()

    def _update_display(self) -> None:
        if not self.current_stage or not self.current_turn:
            return
        stage = self.current_stage
        turn = self.current_turn
        turn_data = self.data[stage].get(turn, {})

        # Summary bar
        turns = sorted(self.data[stage].keys())
        total_in = sum(
            self.data[stage][t].get("resp", {}).get("usage", {}).get("prompt_tokens", 0)
            for t in turns
        )
        total_out = sum(
            self.data[stage][t].get("resp", {}).get("usage", {}).get("completion_tokens", 0)
            for t in turns
        )
        total_cached = sum(
            self.data[stage][t].get("resp", {}).get("usage", {}).get("cached_tokens", 0) or 0
            for t in turns
        )
        stage_name = STAGE_NAMES.get(stage, stage)
        summary = (
            f"{stage_name}  │  Turns: {len(turns)}  │  "
            f"Input: {total_in:,}  Cached: {total_cached:,}  Output: {total_out:,}  │  "
            f"Viewing turn {turn}/{max(turns)}"
        )
        self.query_one("#stage-summary", Static).update(summary)

        # Build content
        text = build_turn_text(
            turn_data, stage, turn, self.data,
            show_full_system=self.show_full_system,
            truncate_tool_results=self.truncate_tool_results,
        )

        content_area = self.query_one("#content-area", TextArea)
        content_area.load_text(text)

        # Status
        resp = turn_data.get("resp", {})
        usage = resp.get("usage", {})
        status_parts = []
        if usage:
            status_parts.append(format_usage(usage))
        status_parts.append(f"[Trunc: {'ON' if self.truncate_tool_results else 'OFF'}]")
        self.query_one("#status-bar", Static).update(" │ ".join(status_parts))

    def action_next_turn(self) -> None:
        if not self.current_stage:
            return
        turns = sorted(self.data[self.current_stage].keys())
        idx = turns.index(self.current_turn) if self.current_turn in turns else 0
        if idx < len(turns) - 1:
            self.current_turn = turns[idx + 1]
            self._update_display()

    def action_prev_turn(self) -> None:
        if not self.current_stage:
            return
        turns = sorted(self.data[self.current_stage].keys())
        idx = turns.index(self.current_turn) if self.current_turn in turns else 0
        if idx > 0:
            self.current_turn = turns[idx - 1]
            self._update_display()

    def action_next_stage(self) -> None:
        if not self.current_stage:
            return
        idx = self.stages_sorted.index(self.current_stage)
        if idx < len(self.stages_sorted) - 1:
            self.current_stage = self.stages_sorted[idx + 1]
            self.current_turn = min(self.data[self.current_stage].keys())
            self._update_display()

    def action_prev_stage(self) -> None:
        if not self.current_stage:
            return
        idx = self.stages_sorted.index(self.current_stage)
        if idx > 0:
            self.current_stage = self.stages_sorted[idx - 1]
            self.current_turn = min(self.data[self.current_stage].keys())
            self._update_display()

    def action_toggle_truncate(self) -> None:
        self.truncate_tool_results = not self.truncate_tool_results
        self._update_display()

    def action_full_system(self) -> None:
        self.show_full_system = not self.show_full_system
        self._update_display()


def find_dump_dir(path: Path) -> Path:
    """If path contains JSON dumps directly, use it. Otherwise list subdirs and let user pick."""
    json_files = list(path.glob("s*_*_*.json"))
    if json_files:
        return path

    subdirs = sorted(
        [d for d in path.iterdir() if d.is_dir() and any(d.glob("s*_*_*.json"))],
        key=lambda d: d.name,
    )
    if not subdirs:
        print(f"Error: no conversation dumps found in {path}")
        sys.exit(1)

    if len(subdirs) == 1:
        print(f"Using: {subdirs[0].name}")
        return subdirs[0]

    print(f"Available dumps in {path}:\n")
    for i, d in enumerate(subdirs):
        n_files = len(list(d.glob("s*_*_*.json")))
        print(f"  [{i}] {d.name}  ({n_files} files)")
    print()
    choice = input(f"Select [0-{len(subdirs)-1}]: ").strip()
    try:
        idx = int(choice)
        return subdirs[idx]
    except (ValueError, IndexError):
        print("Invalid choice")
        sys.exit(1)


def main():
    if len(sys.argv) < 2:
        print("Usage: conversation_viewer.py <dump_directory>")
        print("Example: python3 scripts/conversation_viewer.py /srv/nipa-sashiko/sashiko-dump/")
        sys.exit(1)

    path = Path(sys.argv[1])
    if not path.is_dir():
        print(f"Error: {path} is not a directory")
        sys.exit(1)

    dump_dir = find_dump_dir(path)
    app = ConversationViewer(dump_dir)
    app.run()


if __name__ == "__main__":
    main()
