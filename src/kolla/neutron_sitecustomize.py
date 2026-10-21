# Written by farm and mounted as the L3 agent's sitecustomize.py.
# The guest kernel has no xt_connmark, so iptables-restore rejects every router's
# rules. Neutron only uses CONNMARK to keep address scopes apart, which farms do
# not configure, so those rules are dropped when router_info is imported.
import importlib.abc
import importlib.machinery
import sys

TARGET = "neutron.agent.l3.router_info"
EDITS = [
    (
        "        iptables_manager.ipv4['mangle'].add_rule(\n"
        "            'PREROUTING', copy_address_scope_for_existing)\n",
        "",
    ),
    (
        "        iptables_manager.ipv4['mangle'].add_rule(\n"
        "            'float-snat', save_mark_to_connmark)\n",
        "",
    ),
    (
        "        iptables_manager.ipv6['mangle'].add_rule(\n"
        "            'PREROUTING', copy_address_scope_for_existing)\n",
        "",
    ),
    (
        "        iptables_manager.ipv4['mangle'].add_rule('POSTROUTING', rule)\n",
        "",
    ),
    (
        "        iptables_manager.ipv4['nat'].add_rule('snat', rule)\n",
        "",
    ),
]


class _Loader(importlib.machinery.SourceFileLoader):
    def get_code(self, fullname):
        source = self.get_data(self.path).decode()
        for old, new in EDITS:
            if source.count(old) != 1:
                raise RuntimeError(
                    "farm: unexpected " + self.path + "; review the connmark patch")
            source = source.replace(old, new)
        return compile(source, self.path, "exec", dont_inherit=True)


class _Finder(importlib.abc.MetaPathFinder):
    def find_spec(self, fullname, path, target=None):
        if fullname != TARGET:
            return None
        spec = importlib.machinery.PathFinder.find_spec(fullname, path)
        if spec is not None and spec.origin and spec.origin.endswith(".py"):
            spec.loader = _Loader(fullname, spec.origin)
        return spec


sys.meta_path.insert(0, _Finder())
