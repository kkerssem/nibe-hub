#!/usr/bin/env python3
"""Minimale MCP-client (streamable HTTP) om de server te testen zonder Claude.

    python3 tools/mcp_client.py http://127.0.0.1:8080/mcp TOKEN tools
    python3 tools/mcp_client.py http://127.0.0.1:8080/mcp TOKEN call status '{}'
    python3 tools/mcp_client.py http://127.0.0.1:8080/mcp TOKEN call series '{"ids":[1801],"from":"-1h","bucket":"5m"}'
"""
import json, sys, urllib.request

url, token, cmd = sys.argv[1], sys.argv[2], sys.argv[3]
sid = None


def post(payload):
    global sid
    h = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream",
         "Authorization": f"Bearer {token}"}
    if sid:
        h["Mcp-Session-Id"] = sid
    req = urllib.request.Request(url, json.dumps(payload).encode(), h)
    with urllib.request.urlopen(req, timeout=60) as r:
        sid = r.headers.get("Mcp-Session-Id", sid)
        body = r.read().decode()
    for line in body.splitlines():
        if line.startswith("data:") and line[5:].strip():
            return json.loads(line[5:])
    return None


post({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
    "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}})
post({"jsonrpc": "2.0", "method": "notifications/initialized"})
if cmd == "tools":
    r = post({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
    for t in r["result"]["tools"]:
        print(f"- {t['name']}: {t['description'][:90]}...  params={list(t['inputSchema'].get('properties', {}))}")
else:
    name, args = sys.argv[4], json.loads(sys.argv[5] if len(sys.argv) > 5 else "{}")
    r = post({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": name, "arguments": args}})
    res = r.get("result") or r
    txt = res["content"][0]["text"] if res.get("content") else json.dumps(res)
    print("ERROR:" if res.get("isError") else "OK:")
    try:
        print(json.dumps(json.loads(txt), indent=1, ensure_ascii=False)[:6000])
    except Exception:
        print(txt[:6000])
