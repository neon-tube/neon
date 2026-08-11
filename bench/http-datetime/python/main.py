#!/usr/bin/env python3
"""http-datetime server: answers any request with the current UTC time in ISO 8601."""
import os
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    # HTTP/1.1 gives us persistent (keep-alive) connections.
    protocol_version = "HTTP/1.1"

    def _respond(self):
        # Consume any request body so the connection can be reused.
        length = int(self.headers.get("Content-Length", 0) or 0)
        if length:
            self.rfile.read(length)
        body = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    # Any method returns the same thing.
    do_GET = _respond
    do_POST = _respond
    do_PUT = _respond
    do_DELETE = _respond
    do_HEAD = _respond
    do_OPTIONS = _respond
    do_PATCH = _respond

    def log_message(self, *args):
        pass


def main():
    port = int(os.environ.get("PORT", "18080"))
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    server.serve_forever()


if __name__ == "__main__":
    main()
