#!/usr/bin/env bash

set -o nounset
set -o pipefail

SHELL="/bin/bash"
PATH="/sbin:/bin:/usr/sbin:/usr/bin"

function purge_volume_groups() {
    echo "* Purging volume groups..."

    vgcount=$(find /dev/mapper/ -type l | wc -l)
    if [ ${vgcount} -gt 0 ]; then
        vgs=$(find /dev/mapper/ -type l)
        for vg in ${vgs}; do
            echo ${vg}
            dmsetup remove --force ${vg} >/dev/null 2>&1
            log_and_reboot_on_error "${?}" "Unable to purge volume groups."
        done
    fi
}

function purge_partitions() {
    echo "* Purging partitions..."

    # Destroy master boot record and partition table
    large_drives=($(lsblk -nld -o NAME,SIZE | grep 'T$' | grep -o '^\S*'))

    if [ ! -z "${large_drives}" ]; then
        for drive in $(echo ${large_drives[@]}); do
            wipefs --all --force "/dev/${drive}"
            log_and_reboot_on_error "${?}" "Unable to purge partitions on drive: /dev/${drive}"
        done
    fi
}

function setup_storage() {
    # Create PVs on each additional drive
    large_drives=($(lsblk -nld -o NAME,SIZE | grep 'T$' | grep -o '^\S*'))
    for drive in $(echo ${large_drives[@]}); do
        # Avoid creating PV on main disk
        if [ "/dev/${drive}" == "/dev/nvme0n1" ]; then
            continue
        fi

        test -b "/dev/${drive}"
        log_and_reboot_on_error "${?}" "Drive '/dev/${drive}' not found. Are all drives correctly installed?"

        pvcreate "/dev/${drive}"
        log_and_reboot_on_error "${?}" "Unable to setup PV on drive '/dev/${drive}'."
    done
}

# Establish run order
main() {
    source /opt/ic/bin/functions.sh
    log_start "$(basename $0)"
    purge_volume_groups
    purge_partitions
    setup_storage
    log_end "$(basename $0)"
}

main
