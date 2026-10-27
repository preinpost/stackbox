#!/usr/bin/env python3
"""Exercise a ready farm's public APIs without printing or saving credentials."""

import argparse
import hashlib
import http.client
import http.cookiejar
import json
import os
import re
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


def request(url, method="GET", body=None, token=None, content_type="application/json"):
    headers = {"Content-Type": content_type}
    if token:
        headers["X-Auth-Token"] = token
    if isinstance(body, dict):
        body = json.dumps(body).encode()
    # These are direct service endpoints; shell proxy settings must not reroute them.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(urllib.request.Request(url, body, headers, method=method), timeout=60) as response:
        return response.status, response.headers, response.read()


def verify(endpoints, restart=None):
    credentials = endpoints["credentials"]
    identity = endpoints["services"]["keystone"].rstrip("/")
    glance = endpoints["services"]["glance"].rstrip("/")
    auth = {"auth": {
        "identity": {"methods": ["password"], "password": {"user": {
            "name": credentials["OS_USERNAME"],
            "domain": {"name": credentials.get("OS_USER_DOMAIN_NAME", "Default")},
            "password": credentials["OS_PASSWORD"],
        }}},
        "scope": {"project": {
            "name": credentials["OS_PROJECT_NAME"],
            "domain": {"name": credentials.get("OS_PROJECT_DOMAIN_NAME", "Default")},
        }},
    }}
    status, headers, body = request(identity + "/auth/tokens", "POST", auth)
    assert status == 201, f"token issuance returned {status}"
    token = headers["X-Subject-Token"]
    catalog = json.loads(body)["token"]["catalog"]
    for service_type, expected_url in (("identity", identity), ("image", glance)):
        urls = [endpoint["url"].rstrip("/") for service in catalog
                if service["type"] == service_type for endpoint in service["endpoints"]
                if endpoint["interface"] == "public"]
        if service_type == "identity":
            # Keystone supports both a discovery URL and a versioned auth URL.
            urls = [url.removesuffix("/v3") for url in urls]
            expected_url = expected_url.removesuffix("/v3")
        assert expected_url in urls, f"public {service_type} catalog does not match the published URL"
    print("PASS Keystone: password authentication, scoped token, public service catalog")
    request(identity + "/projects", token=token)
    print("PASS Keystone: authenticated project listing")

    try:
        request(glance + "/v2/images")
    except urllib.error.HTTPError as error:
        assert error.code == 401, f"unauthenticated Glance returned {error.code}"
    else:
        raise AssertionError("Glance accepted an unauthenticated image listing")
    print("PASS Glance: unauthenticated requests rejected")

    image_id = None
    try:
        _, _, body = request(glance + "/v2/images", "POST", {
            "name": "farm-verify-" + str(uuid.uuid4()), "disk_format": "raw",
            "container_format": "bare", "visibility": "private",
        }, token)
        image_id = json.loads(body)["id"]
        image_url = glance + "/v2/images/" + image_id
        data = os.urandom(1024 * 1024)
        request(image_url + "/file", "PUT", data, token, "application/octet-stream")
        for _ in range(30):
            _, _, body = request(image_url, token=token)
            image = json.loads(body)
            if image["status"] == "active":
                break
            time.sleep(1)
        assert image["status"] == "active", f"image status: {image['status']}"
        assert image["size"] == len(data), "uploaded image size differs"
        if restart:
            restart()
            deadline = time.monotonic() + 180
            while True:
                try:
                    status, headers, _ = request(identity + "/auth/tokens", "POST", auth)
                    assert status == 201, f"token issuance returned {status}"
                    token = headers["X-Subject-Token"]
                    request(glance + "/v2/images/" + image_id, token=token)
                    # Every published service must recover, not only the ones checked above.
                    placement = endpoints["services"].get("placement")
                    if placement:
                        request(placement.rstrip("/") + "/resource_providers", token=token)
                    horizon = endpoints["services"].get("horizon")
                    if horizon:
                        request(horizon.rstrip("/") + "/auth/login/")
                    neutron = endpoints["services"].get("neutron")
                    if neutron:
                        request(neutron.rstrip("/") + "/v2.0/networks", token=token)
                    break
                except (urllib.error.URLError, OSError, http.client.HTTPException):
                    if time.monotonic() >= deadline:
                        raise
                    time.sleep(3)
            print("PASS restart: fresh Keystone token and stored Glance image available")
        _, _, downloaded = request(image_url + "/file", token=token)
        assert hashlib.sha256(data).digest() == hashlib.sha256(downloaded).digest(), "image checksum differs"
        print("PASS Glance: 1 MiB upload, active status, download, SHA-256 equality")
    finally:
        if image_id:
            request(glance + "/v2/images/" + image_id, "DELETE", token=token)
    try:
        request(glance + "/v2/images/" + image_id, token=token)
    except urllib.error.HTTPError as error:
        assert error.code == 404, f"deleted image returned {error.code}"
    else:
        raise AssertionError("deleted image is still available")
    print("PASS Glance: image deletion confirmed with HTTP 404")

    services = endpoints["services"]
    if "placement" in services:
        status, _, body = request(services["placement"].rstrip("/") + "/resource_providers",
                                  token=token, content_type="application/json")
        assert status == 200 and "resource_providers" in json.loads(body), "placement listing failed"
        print("PASS Placement: authenticated resource provider listing")
    if "horizon" in services:
        verify_horizon(services["horizon"].rstrip("/"), credentials)
    if "neutron" in services:
        _, _, body = request(services["neutron"].rstrip("/") + "/v2.0/networks?name=public", token=token)
        networks = json.loads(body)["networks"]
        assert networks and networks[0]["router:external"], "external network 'public' is missing"
        # Agents report every 30 s and wait for RabbitMQ after a restart, so give them time.
        deadline = time.monotonic() + 180
        while True:
            _, _, body = request(services["neutron"].rstrip("/") + "/v2.0/agents", token=token)
            agents = json.loads(body)["agents"]
            dead = [a["agent_type"] for a in agents if not a["alive"]]
            if len(agents) >= 4 and not dead:
                break
            assert time.monotonic() < deadline, f"neutron agents not alive: {dead or agents}"
            time.sleep(5)
        print(f"PASS Neutron: external network and {len(agents)} live agents")
    if "nova" in services:
        _, _, body = request(services["nova"].rstrip("/") + "/servers/detail", token=token)
        assert "servers" in json.loads(body), "nova server listing failed"
        print("PASS Nova: authenticated server listing")


def verify_horizon(base, credentials):
    """Log in through the published port the way a browser would."""
    jar = http.cookiejar.CookieJar()
    opener = urllib.request.build_opener(
        urllib.request.ProxyHandler({}), urllib.request.HTTPCookieProcessor(jar), NoRedirect)
    with opener.open(base + "/auth/login/", timeout=60) as response:
        assert response.status == 200, f"login page returned {response.status}"
        page = response.read().decode()
    # The login form carries the configured region as a hidden, required field.
    region = re.search(r'name="region" value="([^"]*)"', page)
    csrf = next((c.value for c in jar if c.name == "csrftoken"), None)
    assert csrf, "horizon did not issue a CSRF cookie"
    form = urllib.parse.urlencode({
        "csrfmiddlewaretoken": csrf, "next": "/",
        "username": credentials["OS_USERNAME"], "password": credentials["OS_PASSWORD"],
        "domain": credentials.get("OS_USER_DOMAIN_NAME", "Default"),
        "region": region.group(1) if region else "",
    }).encode()
    login = urllib.request.Request(base + "/auth/login/", form, {"Referer": base + "/auth/login/"})
    try:
        opener.open(login, timeout=60)
    except urllib.error.HTTPError as error:
        location = error.headers.get("Location", "")
        assert error.code == 302 and "/auth/login" not in location, \
            f"horizon login returned {error.code} {location}"
    else:
        raise AssertionError("horizon login did not redirect")
    # The dashboard root redirects a signed-in user to their default panel.
    try:
        opener.open(base + "/", timeout=60)
    except urllib.error.HTTPError as error:
        location = error.headers.get("Location", "")
        assert error.code == 302 and "/auth/login" not in location, \
            f"dashboard returned {error.code} {location}"
        with opener.open(urllib.parse.urljoin(base + "/", location), timeout=60) as response:
            assert response.status == 200, f"dashboard panel returned {response.status}"
    print("PASS Horizon: browser login through the published port")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args):
        return None


def verify_local_containers(nodes, components):
    expected = {
        "all-in-one": ["mariadb", "rabbitmq", "memcached", "cron", "kolla_toolbox",
                       "keystone_api", "keystone_fernet", "keystone_ssh", "glance_api"]
        + (["placement_api"] if "placement" in components else [])
        + (["horizon"] if "horizon" in components else [])
        + (["neutron_server", "neutron_rpc_server", "neutron_periodic_worker",
            "openvswitch_db", "openvswitch_vswitchd", "neutron_openvswitch_agent",
            "neutron_dhcp_agent", "neutron_l3_agent", "neutron_metadata_agent"]
           if "neutron" in components else [])
        + (["nova_api", "nova_metadata", "nova_scheduler", "nova_conductor",
            "nova_compute", "nova_libvirt", "nova_ssh"]
           if "nova" in components else []),
    }
    # Containers whose Kolla images define no health check.
    unchecked = ["cron", "kolla_toolbox", "neutron_metadata_agent", "nova_ssh"]
    deadline = time.monotonic() + 120
    while True:
        failures = []
        count = 0
        for node in nodes:
            try:
                output = subprocess.check_output([
                    "msb", "exec", "--no-stdin", node["name"], "--", "docker", "ps", "-a",
                    "--format", "{{.Names}}|{{.Status}}",
                ], text=True, timeout=10, stderr=subprocess.DEVNULL)
                statuses = dict(line.split("|", 1) for line in output.splitlines())
                for name in expected[node["role"]]:
                    status = statuses.get(name, "missing")
                    healthy = name in unchecked or "(healthy)" in status
                    if not status.startswith("Up ") or not healthy:
                        failures.append(node["name"] + "/" + name + ": " + status)
                    count += 1
            except (OSError, subprocess.SubprocessError):
                failures.append(node["name"] + ": cannot inspect containers")
        if not failures:
            print(f"PASS local containers: {count} running; all service health checks healthy")
            return
        if time.monotonic() >= deadline:
            raise RuntimeError("containers did not recover: " + "; ".join(failures))
        time.sleep(3)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("farm_id")
    parser.add_argument("--state-dir")
    parser.add_argument("--farm-binary", default="target/debug/farm")
    parser.add_argument("--restart-local-vms", action="store_true",
                        help="restart this local farm's VMs after upload to verify persistence")
    args = parser.parse_args()
    command = [args.farm_binary]
    if args.state_dir:
        command += ["--state-dir", args.state_dir]
    restart = None
    if args.restart_local_vms:
        farm = json.loads(subprocess.check_output(command + ["status", args.farm_id, "--json"]))
        if farm.get("host_id") is not None or farm["status"] != "ready":
            parser.error("--restart-local-vms requires a ready local farm")
        nodes = [node["name"] for node in farm["nodes"]]
        if (len(nodes) != 1 or farm["nodes"][0].get("role") != "all-in-one"
                or nodes[0] not in (args.farm_id, args.farm_id + "-all-in-one")):
            parser.error("unexpected local farm nodes")

        def restart():
            print("Restarting local farm VMs after image upload...", flush=True)
            subprocess.run(["msb", "restart", "--timeout", "90", *nodes], check=True, timeout=120)

    endpoints = json.loads(subprocess.check_output(command + ["endpoints", args.farm_id, "--json"]))
    verify(endpoints, restart)
    if args.restart_local_vms:
        components = farm["plan"]["components"] if farm.get("plan") else ["keystone", "glance"]
        verify_local_containers(farm["nodes"], components)


if __name__ == "__main__":
    main()
