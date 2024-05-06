# Local benchmarking

## CPU Isolation

```shell
sudo /usr/bin/cgconfigparser -l benches/cgconfig.conf -s 1664
sudo cgexec -g cpuset:client-ztunnel fortio server
 ps -A -o psr,cmd | sort -n | rg '[01] fortio' -C 25
```

## Namespace isolation