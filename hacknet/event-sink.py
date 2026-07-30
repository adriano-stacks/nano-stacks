#!/usr/bin/env python3
"""Record the events a stacks-node posts, as the fixture capture wants them.

Hacknet gives its nodes one event observer, the signer, and its keys carry no
`new_block`. Nothing therefore writes the per-transaction receipts that
`cargo xtask capture-fixtures` reads, which is why a capture needs this.

Each `new_block` lands as `new_block/<height>-<hash>.json`, the name the
capture looks for. Other events are acknowledged and dropped: a node whose
observer refuses an event will retry it forever.
"""

import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Sink(BaseHTTPRequestHandler):
    def do_POST(self):  # noqa: N802 - the name http.server dispatches on
        body = self.rfile.read(int(self.headers.get("content-length", 0)))
        if self.path.rstrip("/").endswith("new_block"):
            self.save(body)
        self.send_response(200)
        self.send_header("content-length", "0")
        self.end_headers()

    def save(self, body):
        try:
            block = json.loads(body)
            name = "{:08d}-{}.json".format(
                block["block_height"], block["block_hash"].removeprefix("0x")
            )
        except (ValueError, KeyError) as error:
            print(f"ignoring an unreadable new_block: {error}", file=sys.stderr)
            return
        directory = os.path.join(OUT, "new_block")
        os.makedirs(directory, exist_ok=True)
        # Write and rename, so a capture never reads a half-written event.
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
    print(f"recording new_block events under {OUT} on port {port}", flush=True)
    ThreadingHTTPServer(("0.0.0.0", port), Sink).serve_forever()
