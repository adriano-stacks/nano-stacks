#!/usr/bin/env python3
"""Record the events a stacks-node posts, as the fixture capture wants them.

Hacknet gives its nodes one event observer, the signer, and its keys carry no
`new_block`. Nothing therefore writes the per-transaction receipts that
`cargo xtask capture-fixtures` reads, which is why a capture needs this.

Each `new_block` lands as `new_block/<height>-<hash>.json`, the name the
capture looks for. Every other event lands as `<name>/<sequence>.json`, so a
run can be asked what a node *announced* and not only what it executed: an
observer's half of the RPC surface is answered by files rather than by a log
line, and nano's files and stacks-core's can be read side by side.
"""

import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Events arrive concurrently, and two of a kind must not take the same name.
SEQUENCE = {}
LOCK = threading.Lock()


def next_name(event):
    with LOCK:
        SEQUENCE[event] = SEQUENCE.get(event, 0) + 1
        return "{:08d}.json".format(SEQUENCE[event])


class Sink(BaseHTTPRequestHandler):
    def do_POST(self):  # noqa: N802 - the name http.server dispatches on
        body = self.rfile.read(int(self.headers.get("content-length", 0)))
        event = self.path.rstrip("/").rsplit("/", 1)[-1]
        if event == "new_block":
            self.save_block(body)
        elif event:
            self.write(os.path.join(OUT, event), next_name(event), body)
        self.send_response(200)
        self.send_header("content-length", "0")
        self.end_headers()

    def save_block(self, body):
        try:
            block = json.loads(body)
            name = "{:08d}-{}.json".format(
                block["block_height"], block["block_hash"].removeprefix("0x")
            )
        except (ValueError, KeyError) as error:
            print(f"ignoring an unreadable new_block: {error}", file=sys.stderr)
            return
        self.write(os.path.join(OUT, "new_block"), name, body)

    def write(self, directory, name, body):
        os.makedirs(directory, exist_ok=True)
        # Write and rename, so a reader never sees a half-written event.
        path = os.path.join(directory, name)
        with open(f"{path}.partial", "wb") as handle:
            handle.write(body)
        os.replace(f"{path}.partial", path)

    def log_message(self, *_):
        pass


if __name__ == "__main__":
    OUT = sys.argv[1]
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 3800
    os.makedirs(OUT, exist_ok=True)
    print(f"recording events under {OUT} on port {port}", flush=True)
    ThreadingHTTPServer(("0.0.0.0", port), Sink).serve_forever()
