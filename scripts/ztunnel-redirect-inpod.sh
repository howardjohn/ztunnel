#!/bin/bash
# shellcheck disable=SC2086
# This script sets up redirection in the ztunnel network namespace for namespaced tests (tests/README.md)

set -ex

# Below is from config.sh but used in redirect-worker.sh as well
POD_OUTBOUND=15001
POD_INBOUND=15008
POD_INBOUND_PLAINTEXT=15006

# CONNMARK is needed to make original src work. we set conn mark on prerouting. this is will not effect connections
# from ztunnel to outside the pod, which will go on OUTPUT chain.
# as we are in the pod ns, we can use whatever iptables is default.
iptables-restore --wait 10 <<EOF
*mangle
:PREROUTING ACCEPT [0:0]
:INPUT ACCEPT [0:0]
:FORWARD ACCEPT [0:0]
:OUTPUT ACCEPT [0:0]
:POSTROUTING ACCEPT [0:0]

-A PREROUTING -m mark --mark 1337/0xfff -j CONNMARK --set-xmark 0x111/0xfff
-A PREROUTING -p tcp -m tcp --dport $POD_INBOUND -m mark ! --mark 1337/0xfff -j TPROXY --on-port $POD_INBOUND --on-ip 127.0.0.1 --tproxy-mark 0x111/0xfff
-A PREROUTING -p tcp -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
-A PREROUTING ! -d 127.0.0.1/32 -p tcp -m mark ! --mark 1337/0xfff -j TPROXY --on-port $POD_INBOUND_PLAINTEXT --on-ip 127.0.0.1 --tproxy-mark 0x111/0xfff
-A OUTPUT -m connmark --mark 0x111/0xfff -j CONNMARK --restore-mark --nfmask 0xffffffff --ctmask 0xffffffff
COMMIT
*nat
:PREROUTING ACCEPT [0:0]
:INPUT ACCEPT [0:0]
:OUTPUT ACCEPT [0:0]
:POSTROUTING ACCEPT [0:0]
:ISTIO_REDIRECT - [0:0]
-A OUTPUT -p tcp -j ISTIO_REDIRECT
-A ISTIO_REDIRECT -p tcp -m mark --mark 0x111/0xfff -j ACCEPT
-A ISTIO_REDIRECT -p tcp -m mark ! --mark 1337/0xfff -j REDIRECT --to-ports $POD_OUTBOUND
COMMIT
EOF

ip route add local 0.0.0.0/0 dev lo table 100 || :

# tproxy and original src
ip rule add fwmark 0x111/0xfff pref 32764 lookup 100 || :
