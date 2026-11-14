#!/usr/bin/env python3
"""MCP stdio server with the tool schemas that break providers.

Used by scripts/acceptance.sh for the `heavy` case: it gives the harness a
tool set whose input schemas carry top-level `oneOf`, `anyOf` and `allOf`
members with `$ref` branches, as the Codex Computer Use plugin does. The
model must call `lookup_record` to learn the value the prompt asks for.

Usage: mcp_tools.py --value <text>

Standard library only. Speaks newline-delimited JSON-RPC 2.0 over stdio.
"""

import argparse
import json
import sys

RECORD_ID = 7

LOOKUP_RECORD = {
    "name": "lookup_record",
    "description": "Look up one record by id or by name and return its value.",
    "annotations": {"readOnlyHint": True},
    "inputSchema": {
        "type": "object",
        "properties": {
            "id": {"type": "integer", "description": "Record id."},
            "name": {"type": "string", "description": "Record name."},
        },
        "oneOf": [{"required": ["id"]}, {"required": ["name"]}],
        "additionalProperties": False,
    },
}

JS = {
    "name": "js",
    "description": "Run JavaScript in a persistent REPL or inspect its variables.",
    "inputSchema": {
        "type": "object",
        "$defs": {
            "run": {
                "type": "object",
                "properties": {
                    "code": {"type": "string", "description": "Source to evaluate."},
                    "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 600000},
                },
                "required": ["code"],
                "additionalProperties": False,
            },
            "inspect": {
                "type": "object",
                "properties": {
                    "variable": {"type": "string", "description": "Variable to print."},
                    "depth": {"type": "integer", "minimum": 0, "maximum": 10},
                },
                "required": ["variable"],
                "additionalProperties": False,
            },
        },
        "oneOf": [{"$ref": "#/$defs/run"}, {"$ref": "#/$defs/inspect"}],
    },
}

SEARCH = {
    "name": "search_notes",
    "description": "Search notes by free text, by tag, or both.",
    "inputSchema": {
        "type": "object",
        "properties": {
            "query": {"type": "string"},
            "tag": {"type": "string"},
            "limit": {"type": "integer", "minimum": 1, "maximum": 50},
        },
        "anyOf": [{"required": ["query"]}, {"required": ["tag"]}],
    },
}

CONFIGURE = {
    "name": "configure_notes",
    "description": "Change the notes settings.",
    "inputSchema": {
        "type": "object",
        "allOf": [
            {"properties": {"theme": {"type": "string", "enum": ["light", "dark"]}}},
            {"properties": {"page_size": {"type": "integer", "minimum": 1}}, "required": ["page_size"]},
        ],
    },
}

CALENDAR = {
    "name": "calendar_event",
    "description": "Create, update or delete a calendar event.",
    "inputSchema": {
        "type": "object",
        "properties": {
            "action": {"type": "string", "enum": ["create", "update", "delete"]},
            "event_id": {"type": "string"},
            "title": {"type": "string"},
            "start": {"type": "string", "format": "date-time"},
            "end": {"type": "string", "format": "date-time"},
            "attendees": {"type": "array", "items": {"type": "string", "format": "email"}},
            "recurrence": {
                "type": "object",
                "properties": {
                    "frequency": {"type": "string", "enum": ["daily", "weekly", "monthly"]},
                    "interval": {"type": "integer", "minimum": 1},
                    "until": {"type": "string", "format": "date"},
                },
                "required": ["frequency"],
            },
        },
        "required": ["action"],
        "oneOf": [
            {"properties": {"action": {"const": "create"}}, "required": ["title", "start", "end"]},
            {"properties": {"action": {"const": "update"}}, "required": ["event_id"]},
            {"properties": {"action": {"const": "delete"}}, "required": ["event_id"]},
        ],
    },
}


def plain_tool(name, description, fields):
    return {
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": {field: {"type": kind, "description": doc} for field, (kind, doc) in fields.items()},
            "required": list(fields),
            "additionalProperties": False,
        },
    }


TOOLS = [
    LOOKUP_RECORD,
    JS,
    SEARCH,
    CONFIGURE,
    CALENDAR,
    plain_tool("read_note", "Read one note.", {"note_id": ("string", "Note id.")}),
    plain_tool("write_note", "Create or replace a note.", {"note_id": ("string", "Note id."), "body": ("string", "Markdown body.")}),
    plain_tool("list_notes", "List notes in a folder.", {"folder": ("string", "Folder path.")}),
    plain_tool("move_note", "Move a note to another folder.", {"note_id": ("string", "Note id."), "folder": ("string", "Target folder.")}),
    plain_tool("tag_note", "Add a tag to a note.", {"note_id": ("string", "Note id."), "tag": ("string", "Tag.")}),
    plain_tool("share_note", "Share a note with a user.", {"note_id": ("string", "Note id."), "user": ("string", "User email.")}),
    plain_tool("export_notes", "Export a folder as a zip archive.", {"folder": ("string", "Folder path."), "format": ("string", "zip or tar.")}),
]


def text(value):
    return {"content": [{"type": "text", "text": value}]}


def call_tool(name, arguments, value):
    if name == "lookup_record":
        if arguments.get("id") == RECORD_ID or arguments.get("name") == "acceptance":
            return text(json.dumps({"id": RECORD_ID, "name": "acceptance", "value": value}))
        return text(json.dumps({"error": "no such record"}))
    return text(json.dumps({"ok": True, "tool": name, "arguments": arguments}))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--value", required=True, help="value lookup_record returns for record 7")
    args = parser.parse_args()

    out = sys.stdout
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        message = json.loads(line)
        method = message.get("method")
        request_id = message.get("id")
        if request_id is None:
            continue  # a notification
        params = message.get("params") or {}
        if method == "initialize":
            result = {
                "protocolVersion": params.get("protocolVersion", "2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "rayline-acceptance-tools", "version": "1"},
            }
        elif method == "ping":
            result = {}
        elif method == "tools/list":
            result = {"tools": TOOLS}
        elif method == "tools/call":
            result = call_tool(params.get("name"), params.get("arguments") or {}, args.value)
        elif method in ("resources/list", "resources/templates/list"):
            result = {"resources": [], "resourceTemplates": []}
        elif method == "prompts/list":
            result = {"prompts": []}
        else:
            out.write(json.dumps({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": f"unknown method {method}"}}) + "\n")
            out.flush()
            continue
        out.write(json.dumps({"jsonrpc": "2.0", "id": request_id, "result": result}) + "\n")
        out.flush()


if __name__ == "__main__":
    main()
