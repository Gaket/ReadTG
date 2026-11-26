"""Offline test and optional Linux footprint measurement for the actual MCP binary."""

import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from http.client import HTTPConnection
import json
import os
from pathlib import Path
import re
import select
import stat
import subprocess
from tempfile import TemporaryDirectory
import time


def environment(directory):
    return {
        **{key: value for key, value in os.environ.items() if not key.startswith("TELEGRAM_")},
        "TELEGRAM_READER_DIR": str(directory),
        "TELEGRAM_MCP_PORT": "0",
    }


@contextmanager
def server(binary):
    with TemporaryDirectory() as directory:
        with subprocess.Popen(
            [binary, "serve"], env=environment(directory), stderr=subprocess.PIPE,
            stdout=subprocess.PIPE, text=True,
        ) as process:
            try:
                ready, _, _ = select.select([process.stderr], [], [], 10)
                assert ready, "server did not start"
                address = re.fullmatch(
                    r"Telegram MCP listening at http://127\.0\.0\.1:(\d+)/mcp\n",
                    process.stderr.readline(),
                )
                assert address, "server startup failed"
                path = Path(directory, "mcp-token")
                assert stat.S_IMODE(path.stat().st_mode) == 0o600
                token = path.read_text()
                assert re.fullmatch("[a-f0-9]{64}", token)
                assert not Path(directory, "account.session").exists()
                yield process.pid, int(address[1]), token
            finally:
                process.terminate()
                process.wait(timeout=5)
                assert process.stdout.read() == ""


class Client:
    def __init__(self, port, token):
        self.connection = HTTPConnection("127.0.0.1", port, timeout=5)
        self.token = token
        self.next_id = 0
        self.extra = {}

    def request(self, method, params=None, headers=None, notification=False):
        self.next_id += 1
        body = {"jsonrpc": "2.0", "method": method}
        if not notification:
            body["id"] = self.next_id
        if params is not None:
            body["params"] = params
        request_headers = {
            "Accept": "application/json, text/event-stream",
            "Content-Type": "application/json",
            **self.extra,
        }
        if self.token is not None:
            request_headers["Authorization"] = "Bearer " + self.token
        request_headers.update(headers or {})
        self.connection.request("POST", "/mcp", json.dumps(body), request_headers)
        response = self.connection.getresponse()
        data = response.read()
        if response.headers.get_content_type() == "text/event-stream":
            message = next(line[6:] for line in data.splitlines() if line.startswith(b"data: "))
            data = json.loads(message)
        elif response.headers.get_content_type() == "application/json":
            data = json.loads(data)
        return response.status, response.headers, data

    def initialize(self):
        status, headers, response = self.request("initialize", {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "local-smoke", "version": "1"},
        })
        assert status == 200, (status, response)
        result = response["result"]
        assert result["serverInfo"]["name"] == "telegram-reader-mcp"
        self.extra["MCP-Protocol-Version"] = result["protocolVersion"]
        if session := headers.get("Mcp-Session-Id"):
            self.extra["Mcp-Session-Id"] = session
        status, _, _ = self.request("notifications/initialized", notification=True)
        assert status == 202, status

    def tools(self):
        status, _, response = self.request("tools/list")
        assert status == 200, (status, response)
        return response["result"]["tools"]

    def verify_closed(self, name, arguments):
        status, _, reply = self.request("tools/call", {"name": name, "arguments": arguments})
        assert status == 200, (status, reply)
        result = reply["result"]
        assert result["isError"]
        assert "Chat access is closed" in result["content"][0]["text"]

    def close(self):
        self.connection.close()


def smoke(binary):
    with server(binary) as (_, port, token):
        first = Client(port, token)
        second = Client(port, token)
        unauthorized = Client(port, None)
        invalid = Client(port, "0" * 64)
        try:
            status, _, _ = unauthorized.request("tools/list", headers={"Origin": "https://example.org"})
            assert status == 403, status
            unauthorized.close()
            for client in (unauthorized, invalid):
                status, _, _ = client.request("tools/list")
                assert status == 401, status
            for headers in ({"Origin": "https://example.org"}, {"Origin": "null"}, {"Host": "example.org"}):
                status, _, _ = first.request("tools/list", headers=headers)
                assert status == 403, status
                first.close()

            first.initialize()
            second.initialize()
            assert first.tools() == second.tools()
            status, headers, _ = first.request("tools/list")
            assert status == 200 and headers.get_content_type() == "application/json"
            tools = {entry["name"]: entry for entry in first.tools()}
            assert set(tools) == {"list_chats", "read_messages"}
            for tool in tools.values():
                assert tool["annotations"]["readOnlyHint"] is True
                assert tool["annotations"]["destructiveHint"] is False
            first.verify_closed("list_chats", {})
            second.verify_closed("read_messages", {"chat_id": "123", "query": "word"})
            status, _, response = first.request("tools/call", {"name": "send_message"})
            assert status == 200 and ("error" in response or response["result"]["isError"])
            status, _, _ = first.request("tools/list", {"unused": "x" * 9000})
            assert status == 413, status
        finally:
            for client in (first, second, unauthorized, invalid):
                client.close()

    with TemporaryDirectory() as directory:
        env = environment(directory)
        outcome = subprocess.run(
            [binary, "login"], env=env, stdin=subprocess.DEVNULL, capture_output=True, timeout=5,
        )
        assert outcome.returncode != 0 and b"interactive terminal" in outcome.stderr
        assert not Path(directory, "account.session").exists()

        env["TELEGRAM_ALLOWED_CHATS"] = "all,123"
        outcome = subprocess.run([binary, "serve"], env=env, capture_output=True, timeout=5)
        assert outcome.returncode != 0 and b"comma-separated list" in outcome.stderr
        assert not Path(directory, "mcp-token").exists()

    print("HTTP smoke passed for two independent MCP clients.")


def process_stats(pid):
    status = Path(f"/proc/{pid}/status").read_text()
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    cpu = (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")
    fields = dict(re.findall(r"^(VmRSS|VmHWM):\s+(\d+)", status, re.MULTILINE))
    return cpu, int(fields["VmRSS"]), int(fields["VmHWM"])


def benchmark(binary):
    with server(binary) as (pid, port, token):
        clients = (Client(port, token), Client(port, token))
        try:
            for client in clients:
                client.initialize()
                client.tools()
            idle_cpu, idle_rss, _ = process_stats(pid)
            time.sleep(10)
            start_cpu, start_rss, _ = process_stats(pid)
            start = time.monotonic()

            def workload(client):
                for _ in range(250):
                    assert len(client.tools()) == 2
                    client.verify_closed("read_messages", {"chat_id": "123", "query": "invoice", "limit": 3})

            with ThreadPoolExecutor(max_workers=2) as workers:
                list(workers.map(workload, clients))
            elapsed = time.monotonic() - start
            end_cpu, end_rss, high_rss = process_stats(pid)
        finally:
            for client in clients:
                client.close()

    print(f"idle (10 s): {start_cpu - idle_cpu:.2f} CPU s, RSS {idle_rss}/{start_rss} KiB")
    print(f"two clients (1000 requests, {elapsed:.2f} s): {end_cpu - start_cpu:.2f} CPU s, "
          f"RSS {start_rss}/{end_rss} KiB, process RSS high-water mark {high_rss} KiB")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=lambda path: Path(path).resolve())
    parser.add_argument("--benchmark", action="store_true")
    args = parser.parse_args()
    (benchmark if args.benchmark else smoke)(args.binary)
