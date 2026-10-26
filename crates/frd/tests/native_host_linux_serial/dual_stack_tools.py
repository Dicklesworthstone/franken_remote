#!/usr/bin/python3
"""Synthetic per-address rule evidence, never kernel ingress qualification."""
import fcntl
import ipaddress
import json
import pathlib
import re
import sys

root = pathlib.Path(__file__).parent
assert pathlib.Path('/run/fr-synthetic-ingress').read_text() == 'synthetic-only\n'
assert str(pathlib.Path('/proc/self/ns/net').readlink()) != pathlib.Path('/run/fr-parent-netns').read_text().strip()
# Independent protection supervisors can overlap their read-only refresh tools.
# Serialize the fixture's counters, not the production networking or policy.
with (root / 'state.lock').open('a') as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    state_path = root / 'state.json'
    state = json.loads(state_path.read_text()) if state_path.exists() else {'tables': {}, 'created': 0, 'reads': {}, 'deleted': 0}
    args = sys.argv[1:]
    if pathlib.Path(__file__).name == 'ip':
        assert args == ['-j', 'address', 'show', 'dev', 'fr-fixture']
        print(json.dumps([{'ifindex': 42, 'ifname': 'fr-fixture', 'addr_info': [{'local': '100.64.0.1'}, {'local': 'fd7a:115c:a1e0::1'}]}]))
    elif args == ['-f', '-']:
        script = sys.stdin.read(4096)
        if script.startswith('create table'):
            match = re.fullmatch(r'create table inet (frd_[0-9a-f]{32})\nadd chain inet \1 input \{ type filter hook input priority -310; policy accept; \}\nadd rule inet \1 input (ip6?) daddr ([0-9a-f:.]+) udp dport (\d+) meta iif != 42 drop\n', script)
            assert match, script
            table, family, address, port = match[1], match[2], match[3], int(match[4])
            assert (family, address) in [('ip', '100.64.0.1'), ('ip6', 'fd7a:115c:a1e0::1')]
            assert table not in state['tables']
            assert ipaddress.ip_address(address).version == (4 if family == 'ip' else 6)
            state['tables'][table] = {'address': address, 'rule': {'nftables': [
                {'table': {'family': 'inet', 'name': table}},
                {'chain': {'family': 'inet', 'table': table, 'name': 'input', 'type': 'filter', 'hook': 'input', 'prio': -310, 'policy': 'accept'}},
                {'rule': {'family': 'inet', 'table': table, 'chain': 'input', 'expr': [
                    {'match': {'op': '==', 'left': {'payload': {'protocol': family, 'field': 'daddr'}}, 'right': address}},
                    {'match': {'op': '==', 'left': {'payload': {'protocol': 'udp', 'field': 'dport'}}, 'right': port}},
                    {'match': {'op': '!=', 'left': {'meta': {'key': 'iif'}}, 'right': 42}}, {'drop': None}]}}]}}
            state['created'] += 1
        else:
            match = re.fullmatch(r'delete table inet (frd_[0-9a-f]{32})\n', script)
            assert match and match[1] in state['tables'], script
            failed_cleanup = root / 'fail_cleanup'
            if failed_cleanup.exists() and failed_cleanup.read_text() == state['tables'][match[1]]['address']:
                sys.exit(1)
            del state['tables'][match[1]]
            state['deleted'] += 1
    elif args == ['-j', 'list', 'tables', 'inet']:
        print(json.dumps({'nftables': [{'table': {'family': 'inet', 'name': table}} for table in state['tables']]}))
    else:
        assert len(args) == 6 and args[:5] == ['-j', '-n', 'list', 'table', 'inet'], args
        row = state['tables'][args[5]]
        address = row['address']
        state['reads'][address] = state['reads'].get(address, 0) + 1
        rule = row['rule']
        tamper = root / 'tamper'
        if tamper.exists() and tamper.read_text() == address:
            rule = {'nftables': rule['nftables'][:2]}
        print(json.dumps(rule))
    state_path.write_text(json.dumps(state))
