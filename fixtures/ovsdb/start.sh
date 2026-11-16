#!/bin/bash
# ovs-sandbox owns the private database, dummy daemons and their cleanup.
set -euo pipefail

# The launcher recreates ./sandbox, so always run it in the fresh fixture mount.
cd /fixture
export SHELL=/usr/local/bin/populate-ovs-fixture
exec /bin/sh /usr/local/bin/ovs-sandbox --installed --dummy=override
