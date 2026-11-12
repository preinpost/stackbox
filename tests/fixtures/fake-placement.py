#!/usr/bin/env python3
"""A stand-in for Placement that keeps the parts of its contract the GPU scripts use.

The GPU verification script runs against a real Placement inside the control VM, where a
wrong request shape only shows up minutes into a deployment. This server answers the same
requests with the same shapes, so a test can run the generated Python block itself:

* microversions 1.0 - 1.19 answer `POST /resource_providers` with 201 and an empty body,
  and 1.20 on answers 200 with the new provider; both carry `Location` (which is why a
  script that reads `created["uuid"]` fails here, as it did on the farm);
* a resource provider takes a trait only when the trait table holds it, and
  `PUT /traits/<name>` creates a missing one (201) and accepts an existing one (204);
* a provider's trait list is a list of strings on every microversion.

Usage: `fake-placement.py [port]` prints `http://127.0.0.1:<port>` on stdout and serves
until killed. `GET /_state` returns everything written, for a test to assert the end state.
`GET /_microversions` returns the microversion of every request seen, in order.
"""

import json
import sys
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlparse

LOCK = threading.Lock()
PROVIDERS = {}
TRAITS = set()
SEEN = []


def error(status, message):
    return status, {"errors": [{"status": status, "title": message}]}, {}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        sys.stderr.write("fake-placement: " + fmt % args + "\n")

    def microversion(self):
        # "placement 1.29" -> (1, 29); no header means the minimum, 1.0.
        value = self.headers.get("OpenStack-API-Version", "")
        parts = value.split()
        if len(parts) == 2 and parts[0] == "placement":
            major, _, minor = parts[1].partition(".")
            try:
                return int(major), int(minor or 0)
            except ValueError:
                pass
        return 1, 0

    def body(self):
        length = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(length)) if length else {}

    def answer(self, status, payload=None, headers=None):
        data = b"" if payload is None else json.dumps(payload).encode()
        self.send_response(status)
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        if data:
            self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        if data:
            self.wfile.write(data)

    def provider(self, name):
        for provider in PROVIDERS.values():
            if provider["name"] == name:
                return provider
        return None

    def do_GET(self):
        url = urlparse(self.path)
        path = unquote(url.path)
        if path == "/_state":
            with LOCK:
                return self.answer(
                    200,
                    {
                        "providers": list(PROVIDERS.values()),
                        "traits": sorted(TRAITS),
                        "microversions": SEEN,
                    },
                )
        if path == "/resource_providers":
            wanted = parse_qs(url.query).get("name")
            with LOCK:
                providers = [
                    self.public(provider)
                    for provider in PROVIDERS.values()
                    if not wanted or provider["name"] in wanted
                ]
            return self.answer(200, {"resource_providers": providers})
        parts = path.strip("/").split("/")
        if len(parts) == 2 and parts[0] == "resource_providers":
            with LOCK:
                provider = PROVIDERS.get(parts[1])
                if provider is None:
                    return self.answer(404, {"errors": [{"status": 404}]})
                return self.answer(200, self.public(provider))
        if len(parts) == 3 and parts[0] == "resource_providers":
            with LOCK:
                provider = PROVIDERS.get(parts[1])
                if provider is None:
                    return self.answer(404, {"errors": [{"status": 404}]})
                if parts[2] == "inventories":
                    return self.answer(
                        200,
                        {
                            "inventories": provider["inventories"],
                            "resource_provider_generation": provider["generation"],
                        },
                    )
                if parts[2] == "traits":
                    return self.answer(
                        200,
                        {
                            "traits": provider["traits"],
                            "resource_provider_generation": provider["generation"],
                        },
                    )
                if parts[2] == "allocations":
                    return self.answer(
                        200,
                        {
                            "allocations": {},
                            "resource_provider_generation": provider["generation"],
                        },
                    )
        return self.answer(404, {"errors": [{"status": 404}]})

    def public(self, provider):
        return {
            "uuid": provider["uuid"],
            "name": provider["name"],
            "parent_provider_uuid": provider["parent_provider_uuid"],
            "generation": provider["generation"],
        }

    def do_POST(self):
        url = urlparse(self.path)
        path = unquote(url.path)
        if path != "/resource_providers":
            return self.answer(404, {"errors": [{"status": 404}]})
        body = self.body()
        with LOCK:
            SEEN.append(f"{self.microversion()[0]}.{self.microversion()[1]}")
            if self.provider(body.get("name")):
                return self.answer(409, {"errors": [{"status": 409}]})
            parent = body.get("parent_provider_uuid")
            if parent is not None and parent not in PROVIDERS:
                return self.answer(400, {"errors": [{"status": 400}]})
            provider = {
                "uuid": str(uuid.uuid4()),
                "name": body.get("name"),
                "parent_provider_uuid": parent,
                "generation": 0,
                "inventories": {},
                "traits": [],
            }
            PROVIDERS[provider["uuid"]] = provider
            location = "/resource_providers/" + provider["uuid"]
            if self.microversion() >= (1, 20):
                return self.answer(200, self.public(provider))
            return self.answer(201, None, {"Location": location})

    def do_PUT(self):
        url = urlparse(self.path)
        path = unquote(url.path)
        parts = path.strip("/").split("/")
        if len(parts) == 2 and parts[0] == "traits":
            name = parts[1]
            with LOCK:
                if name in TRAITS:
                    return self.answer(204)
                if not name.startswith("CUSTOM_") or not name[7:].replace("_", "").isalnum():
                    return self.answer(
                        400,
                        {"errors": [{"status": 400, "detail": "invalid trait name"}]},
                    )
                TRAITS.add(name)
            return self.answer(201)
        if len(parts) != 3 or parts[0] != "resource_providers":
            return self.answer(404, {"errors": [{"status": 404}]})
        body = self.body()
        with LOCK:
            provider = PROVIDERS.get(parts[1])
            if provider is None:
                return self.answer(404, {"errors": [{"status": 404}]})
            generation = body.get("resource_provider_generation")
            if generation is not None and generation != provider["generation"]:
                return self.answer(
                    409,
                    {"errors": [{"status": 409, "detail": "generation conflict"}]},
                )
            if parts[2] == "inventories":
                provider["inventories"] = body.get("inventories") or {}
            elif parts[2] == "traits":
                wanted = body.get("traits") or []
                unknown = sorted(set(wanted) - TRAITS)
                if unknown:
                    # The refusal that a farm which never created its own traits hits.
                    return self.answer(
                        400,
                        {
                            "errors": [
                                {
                                    "status": 400,
                                    "detail": "No such trait " + ", ".join(unknown),
                                }
                            ]
                        },
                    )
                provider["traits"] = wanted
            else:
                return self.answer(404, {"errors": [{"status": 404}]})
            provider["generation"] += 1
            return self.answer(
                200,
                {
                    "resource_provider_generation": provider["generation"],
                },
            )
        return self.answer(404, {"errors": [{"status": 404}]})


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    # The compute Nova itself reported, which a simulated farm's child providers hang off.
    root = {
        "uuid": "11111111-1111-1111-1111-111111111111",
        "name": "farm-compute-1",
        "parent_provider_uuid": None,
        "generation": 5,
        "inventories": {"VCPU": {"total": 8}, "MEMORY_MB": {"total": 4096}},
        "traits": [],
    }
    PROVIDERS[root["uuid"]] = root
    print("http://127.0.0.1:%d" % server.server_address[1], flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
