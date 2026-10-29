#!/bin/bash
# per-boot accelerator + memory setup for the lifecycle runs on gnr-qual1
set -u
CONF=$HOME/accel-qual1-20261013b.conf
echo "== disable all enabled dsa/iax devices"
for d in /sys/bus/dsa/devices/dsa[0-9]* /sys/bus/dsa/devices/iax[0-9]*; do
  n=$(basename "$d"); st=$(cat "$d/state" 2>/dev/null)
  [ "$st" = enabled ] || continue
  for w in "$d"/wq*; do [ -e "$w/state" ] && [ "$(cat "$w/state")" = enabled ] && sudo accel-config disable-wq "$n/$(basename "$w")" >/dev/null 2>&1; done
  sudo accel-config disable-device "$n" >/dev/null 2>&1 && echo "  disabled $n"
done
echo "== iaa_crypto: $(sudo rmmod iaa_crypto 2>&1 && echo removed || echo kept)"
echo "== load-config"
sudo accel-config load-config -c "$CONF" -e 2>&1 | tail -5
echo "== enable anything the loader left disabled"
for d in /sys/bus/dsa/devices/dsa[0-9]* /sys/bus/dsa/devices/iax[0-9]*; do
  n=$(basename "$d")
  [ "$(cat "$d/state")" = enabled ] || sudo accel-config enable-device "$n" 2>&1 | tail -1
  for w in "$d"/wq*; do
    [ -e "$w/state" ] || continue
    [ "$(cat "$w/size")" = 0 ] && continue
    [ "$(cat "$w/state")" = enabled ] || sudo accel-config enable-wq "$n/$(basename "$w")" 2>&1 | tail -1
  done
done
sudo chmod 666 /dev/dsa/wq*.0 /dev/iax/wq*.0 /dev/uffd_dma 2>&1
echo "== memory + cpu"
echo 3072 | sudo tee /sys/devices/system/node/node0/hugepages/hugepages-2048kB/nr_hugepages >/dev/null
for g in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do echo performance | sudo tee "$g" >/dev/null; done
sudo sysctl -q vm.unprivileged_userfaultfd=1
echo 0 | sudo tee /sys/kernel/uffd_dma/offloading /sys/module/dcbm/parameters/offloading >/dev/null
echo "== result"
ls /dev/dsa /dev/iax | tr '\n' ' '; echo
echo "hugepages node0: $(cat /sys/devices/system/node/node0/hugepages/hugepages-2048kB/nr_hugepages) free $(cat /sys/devices/system/node/node0/hugepages/hugepages-2048kB/free_hugepages)"
echo "governor: $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor) $(cat /sys/devices/system/cpu/cpu200/cpufreq/scaling_governor)"
echo "uffd sysctl: $(sysctl -n vm.unprivileged_userfaultfd); dma channels: $(ls /sys/class/dma | wc -l)"
for w in /sys/bus/dsa/devices/wq{0,2,4,6}.{0,1} /sys/bus/dsa/devices/wq{1,3,5,7,9,11,13,15}.0; do
  printf "%s %s %s/%s size=%s mts=%s drv=%s\n" "$(basename $w)" "$(cat $w/state)" "$(cat $w/mode)" "$(cat $w/type)" "$(cat $w/size)" "$(cat $w/max_transfer_size)" "$(cat $w/driver_name 2>/dev/null)"
done
