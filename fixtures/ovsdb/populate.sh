#!/bin/bash
# Runs as ovs-sandbox's nested shell with its private OVS_* directories.
set -euo pipefail

# Discovery needs only the database. Stop the dummy switch before populating it
# so status/statistics writers cannot interfere with before/after dump checks.
ovs-appctl -t ovs-vswitchd exit
for ((attempt = 0; attempt < 100; attempt++)); do
    [ -f "$OVS_RUNDIR/ovs-vswitchd.pid" ] || break
    sleep 0.05
done
[ ! -f "$OVS_RUNDIR/ovs-vswitchd.pid" ] || { echo "FAIL: dummy switch did not stop" >&2; exit 1; }

vsctl=(ovs-vsctl --db="unix:$OVS_RUNDIR/db.sock" --timeout=5 --no-wait)
"${vsctl[@]}" add-br br0
"${vsctl[@]}" add-port br0 eth1
"${vsctl[@]}" add-bond br0 bond0 eth2 eth3
"${vsctl[@]}" add-port br0 gre0 -- set interface gre0 type=gre options:remote_ip=192.0.2.10
touch /fixture/ready
exec sleep infinity
