"""A local HTTP server for tests, on 127.0.0.1 and, where the machine has
it, [::1] at the same port. Pages reach it as localhost too, which Windows
tries as ::1 first: with nothing listening there, every such request
started with a refused connection."""

import contextlib
import socket
import threading
from http.server import ThreadingHTTPServer


class _V6(ThreadingHTTPServer):
    address_family = socket.AF_INET6


def serve(handler):
    """Starts serving with `handler`; returns the servers, whose first one
    has the port. Each must be shut down, then closed."""
    servers = [ThreadingHTTPServer(("127.0.0.1", 0), handler)]
    # No IPv6 loopback here: localhost can only be 127.0.0.1.
    with contextlib.suppress(OSError):
        servers.append(_V6(("::1", servers[0].server_port), handler))
    for server in servers:
        threading.Thread(target=server.serve_forever, daemon=True).start()
    return servers
