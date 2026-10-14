"""Owned loopback fixture process. Inherited bound socket; no outbound access."""
import base64
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
from pathlib import Path
import re
import signal
import socket
import sys
import time


FIELD_NAME = re.compile(r"[!#$%&'*+.^_`|~0-9A-Za-z-]+", re.ASCII)
ASCII_LOWER = str.maketrans("ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghijklmnopqrstuvwxyz")
ALLOWED_FIELDS = frozenset({"accept", "accept-encoding", "connection", "host", "user-agent"})


def validate_header_fields(items, expected_authority):
    """A semantic NAME-only view; never mutate captured names/order/values.

    Duplicate fields are rejected before constructing the view (including two
    equal Host values). The ordered raw_items list remains authoritative evidence.
    """
    if type(items) is not list:
        raise ValueError("ordered HTTP field items are required")
    view = {}
    for item in items:
        if type(item) not in (list, tuple) or len(item) != 2:
            raise ValueError("HTTP field item must be a name/value pair")
        name, value = item
        if type(name) is not str or not FIELD_NAME.fullmatch(name) or type(value) is not str:
            raise ValueError("ASCII HTTP field-name token and unchanged string value required")
        key = name.translate(ASCII_LOWER)
        if key in view:
            raise ValueError("duplicate ASCII-case-folded HTTP field: " + key)
        if key not in ALLOWED_FIELDS:
            raise ValueError("HTTP field outside private fixture allowlist: " + name)
        view[key] = value
    if "host" not in view:
        raise ValueError("exactly one Host field is required")
    if view["host"] != expected_authority:
        raise ValueError("Host value differs from exact fixture authority/port")
    return view


def serve(fd, routes_path, log_path):
    routes = json.loads(Path(routes_path).read_bytes())
    running = True
    sequence = 0

    def stop(*_):
        nonlocal running
        running = False

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def handle_one_request(self):
            self.audited = False
            self.raw_requestline = b""
            self.command, self.path, self.headers = None, "", None
            try:
                super().handle_one_request()
            finally:
                if self.raw_requestline and not self.audited:
                    self.audit_request(False, 400)

        def send_error(self, code, message=None, explain=None):
            # Includes parse failures and oversized request lines, which bypass
            # method dispatch. Preserve one record even when inherited logging
            # is suppressed; do not double-log parse_request or GET afterwards.
            if self.raw_requestline and not self.audited:
                self.audit_request(False, code)
            else:
                self.prepared_response = (code, {"Content-Length": "0"}, b"")
            self.send_prepared_response()

        def audit_request(self, parsed, rejected_status=None):
            nonlocal sequence
            words = self.raw_requestline.decode("latin-1").strip().split()
            method = words[0] if words else ""
            path = words[1] if len(words) > 1 else ""
            headers = self.headers if self.headers is not None else {}
            header_items = list(headers.raw_items()) if hasattr(headers, "raw_items") else []
            try:
                validate_header_fields(header_items, "127.0.0.1:" + str(self.server.server_address[1]))
                admitted_headers = True
            except ValueError:
                admitted_headers = False
            route = routes.get(path)
            unsafe = (not parsed or method != "GET" or self.command != method or self.path != path
                      or len(words) != 3 or words[2] not in ("HTTP/1.0", "HTTP/1.1")
                      or route is None or self.client_address[0] != "127.0.0.1"
                      or not admitted_headers)
            body = base64.b64decode(route["body"]["base64"], validate=True) if not unsafe else b""
            status = rejected_status if rejected_status is not None else (405 if method != "GET" else 400) if unsafe else route["status"]
            response_headers = route["headers"] if not unsafe else {"Content-Length": "0"}
            record = {"ordinal": sequence, "method": method, "path": path,
                      "parsed": parsed, "request_line_base64": base64.b64encode(self.raw_requestline).decode("ascii"),
                      "purpose": route.get("purpose", "ctx-api") if route else "unknown",
                      "peer": self.client_address[0], "headers": dict(header_items), "header_items": header_items,
                      "header_items_source": "HTTPMessage.raw_items",
                      "status": status, "response_headers": response_headers,
                      "body_sha256": hashlib.sha256(body).hexdigest(), "body_size": len(body),
                      "unsafe": unsafe, "time_ns": time.time_ns()}
            sequence += 1
            with open(log_path, "ab", buffering=0) as out:
                out.write(json.dumps(record, sort_keys=True).encode("ascii") + b"\n")
            self.audited = True
            self.prepared_response = (status, response_headers, body)
            return not unsafe

        def parse_request(self):
            parsed = super().parse_request()
            if self.audited:  # send_error already retained the rejected parse.
                return False
            admitted = self.audit_request(parsed)
            if not admitted and parsed:
                self.send_prepared_response()
            return parsed and admitted

        def send_prepared_response(self):
            status, headers, body = self.prepared_response
            self.send_response(status)
            for key, value in headers.items():
                self.send_header(key, value)
            self.end_headers()
            self.wfile.write(body)
            self.wfile.flush()
            self.close_connection = True

        def do_GET(self):
            self.send_prepared_response()

    server = HTTPServer(("127.0.0.1", 0), Handler, bind_and_activate=False)
    server.socket.close()
    server.socket = socket.socket(fileno=fd)
    server.server_address = server.socket.getsockname()
    server.timeout = 0.2
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    print(json.dumps({"ready": True, "address": list(server.server_address)}), flush=True)
    try:
        while running:
            server.handle_request()
    finally:
        server.server_close()
    print("SERVER_CLOSED", flush=True)


if __name__ == "__main__":
    serve(int(sys.argv[1]), sys.argv[2], sys.argv[3])
