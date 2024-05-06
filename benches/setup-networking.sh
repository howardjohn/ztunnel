#!/bin/bash
set -ex

# Cleanup
ip link del ztunnel-br0 || :
ip netns del client-node || sleep .5 # sleep on error since recursive cleanup is async
ip netns del server-node || sleep .5

# Setup the host
ip link add ztunnel-br0 type bridge
ip link set dev ztunnel-br0 up
ip addr add 172.173.0.1/16 dev ztunnel-br0

# Setup client-node
ip netns add client-node
ip link add veth0client type veth peer name eth0 netns client-node
ip link set dev veth0client up
ip link set dev veth0client master ztunnel-br0
ip route add 10.0.2.0/24 via 172.173.0.2 dev ztunnel-br0
# Give our node an IP
ip -n client-node link set dev eth0 up
ip -n client-node addr add 172.173.0.2/16 dev eth0
# Route everything to the network
ip -n client-node route add default via 172.173.0.1
ip -n client-node route add 172.173.0.0/17 dev eth0 scope link src 172.173.0.2
ip -n client-node link set lo up
ip netns exec client-node iptables -w -t nat -A OUTPUT -p tcp ! -o lo -m mark ! --mark 1337 -j REDIRECT --to-ports 15001

# Setup server-node
ip netns add server-node
ip link add veth0server type veth peer name eth0 netns server-node
ip link set dev veth0server up
ip link set dev veth0server master ztunnel-br0
ip route add 10.0.3.0/24 via 172.173.0.3 dev ztunnel-br0
# Give our node an IP
ip -n server-node link set dev eth0 up
ip -n server-node addr add 172.173.0.3/16 dev eth0
# Route everything to the network
ip -n server-node route add default via 172.173.0.1
ip -n server-node route add 172.173.0.0/17 dev eth0 scope link src 172.173.0.3
ip -n server-node link set lo up
