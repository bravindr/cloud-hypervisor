#!/bin/bash
# After an idxd reload: the IAA user WQs the daemon (QPL) uses. load-config
# does not restore max_transfer_size for them and the 2 MiB default makes
# 2 MiB jobs time out (QPL status 319); device nodes come back root-only.
for d in 1 3 5 7 9 11 13 15; do
  w=/sys/bus/dsa/devices/wq$d.0
  sudo accel-config disable-wq iax$d/wq$d.0 >/dev/null 2>&1
  for kv in "group_id 0" "mode shared" "threshold 248" "type user" "name chsnap-iaa" "driver_name user" "block_on_fault 1" "priority 10" "max_transfer_size 1073741824"; do
    set -- $kv; echo $2 | sudo tee $w/$1 >/dev/null
  done
  sudo accel-config enable-device iax$d >/dev/null 2>&1
  sudo accel-config enable-wq iax$d/wq$d.0
done
sudo chmod 666 /dev/iax/wq*.0 /dev/dsa/wq*.0
