#!/usr/bin/python3
"""Explicit SYNTHETIC ip/nft command responses, not firewall qualification.

Used only inside the disposable namespace created by the companion runner. Real
UDP/TLS and the real Boundary/serial/Host owners execute; this fixture models the
privileged host's interface and rule evidence, not kernel packet filtering.
"""
import json
import pathlib
import re
import sys

root = pathlib.Path(__file__).parent
assert pathlib.Path('/run/fr-synthetic-ingress').read_text() == 'synthetic-only\n'
assert str(pathlib.Path('/proc/self/ns/net').readlink()) != pathlib.Path('/run/fr-parent-netns').read_text().strip()
state_path = root / 'state.json'
state = json.loads(state_path.read_text()) if state_path.exists() else {'table': None, 'created': 0, 'reads': 0, 'deleted': 0}
args = sys.argv[1:]
if pathlib.Path(__file__).name == 'ip':
    assert args == ['-j', 'address', 'show', 'dev', 'fr-fixture']
    print(json.dumps([{'ifindex': 42, 'ifname': 'fr-fixture', 'addr_info': [{'local': '100.64.0.1'}]}]))
elif args == ['-f', '-']:
    script = sys.stdin.read(4096)
    if script.startswith('create table'):
        m = re.fullmatch(r'create table inet (frd_[0-9a-f]{32})\nadd chain inet \1 input \{ type filter hook input priority -310; policy accept; \}\nadd rule inet \1 input ip daddr 100\.64\.0\.1 udp dport (\d+) meta iif != 42 drop\n', script)
        assert m and state['table'] is None, script
        table, port = m[1], int(m[2])
        state['table'] = table
        state['created'] += 1
        state['rule'] = {'nftables': [
            {'table': {'family': 'inet', 'name': table}},
            {'chain': {'family': 'inet', 'table': table, 'name': 'input', 'type': 'filter', 'hook': 'input', 'prio': -310, 'policy': 'accept'}},
            {'rule': {'family': 'inet', 'table': table, 'chain': 'input', 'expr': [
                {'match': {'op': '==', 'left': {'payload': {'protocol': 'ip', 'field': 'daddr'}}, 'right': '100.64.0.1'}},
                {'match': {'op': '==', 'left': {'payload': {'protocol': 'udp', 'field': 'dport'}}, 'right': port}},
                {'match': {'op': '!=', 'left': {'meta': {'key': 'iif'}}, 'right': 42}}, {'drop': None}]}}]}
    else:
        assert script == f'delete table inet {state["table"]}\n', script
        state['table'] = None
        state['deleted'] += 1
elif args == ['-j', 'list', 'tables', 'inet']:
    print(json.dumps({'nftables': [] if state['table'] is None else [{'table': {'family': 'inet', 'name': state['table']}}]}))
else:
    assert args == ['-j', '-n', 'list', 'table', 'inet', state['table']], args
    state['reads'] += 1
    rule = state['rule']
    if (root / 'tamper').exists():
        rule = {'nftables': rule['nftables'][:2]}
    print(json.dumps(rule))
state_path.write_text(json.dumps(state))
