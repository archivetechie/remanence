#!/usr/bin/env bash
# Give Remanence's CI tests a disposable ext4 journal volume marked write-through.
# The product's journal-volume trust rule is unchanged; this sets up the runner.
set -euo pipefail

fail() {
    printf 'ci_journal_volume.sh: %s\n' "$*" >&2
    exit 1
}

if [[ $# -lt 2 ]]; then
    fail 'usage: ci_journal_volume.sh up <mount point> <size> | down <mount point>'
fi
mode=$1
case "$mode" in
    up) [[ $# -eq 3 ]] || fail 'up requires a mount point and size' ;;
    down) [[ $# -eq 2 ]] || fail 'down requires a mount point' ;;
    *) fail "unknown subcommand: $mode" ;;
esac

mount_point=$2
[[ $mount_point == /* && $mount_point != *[$'\r\n']* ]] ||
    fail 'mount point must be an absolute path without a newline'
while [[ $mount_point == */ && $mount_point != / ]]; do
    mount_point=${mount_point%/}
done
[[ $mount_point != / ]] || fail 'refusing the root mount point'
image="$mount_point.img"
loop_device=
mounted=0
image_created=0
step='prepare volume'

# Roll back only resources acquired by this up. If a release fails, preserve its
# backing resources rather than detach a mounted device or remove an attached image.
finish() {
    local status=$?
    trap - EXIT INT TERM
    if (( status == 0 )); then
        return
    fi
    printf 'ci_journal_volume.sh: %s failed (exit %s)\n' "$step" "$status" >&2
    if [[ $mode == up ]]; then
        if (( mounted )); then
            if ! sudo umount -- "$mount_point"; then
                printf 'cleanup: unmount failed; retaining loop device and image\n' >&2
                exit "$status"
            fi
        fi
        if [[ -n $loop_device ]]; then
            if ! sudo losetup --detach "$loop_device"; then
                printf 'cleanup: loop detach failed; retaining image %s\n' "$image" >&2
                exit "$status"
            fi
        fi
        if (( image_created )); then
            if ! rm -f -- "$image"; then
                printf 'cleanup: could not remove image %s\n' "$image" >&2
            fi
        fi
    fi
    exit "$status"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

case "$mode" in
    up)
        step='check for an existing image'
        if [[ -e $image || -L $image ]]; then
            fail "refusing existing image: $image"
        fi
        step='check for an existing mount'
        if mountpoint -q -- "$mount_point"; then
            fail "refusing existing mount: $mount_point"
        fi
        step='create image exclusively'
        (set -o noclobber; : > "$image")
        image_created=1
        step='size image'
        truncate -s "$3" -- "$image"
        step='format ext4 image'
        mkfs.ext4 -q -F "$image"
        step='attach loop device'
        loop_device=$(sudo losetup --find --show "$image")
        [[ $loop_device =~ ^/dev/loop[0-9]+$ ]] || fail "unexpected loop device: $loop_device"
        cache_path="/sys/block/${loop_device##*/}/queue/write_cache"
        fua_path="/sys/block/${loop_device##*/}/queue/fua"
        step='set loop write cache'
        printf 'write through\n' | sudo tee "$cache_path" > /dev/null
        step='create mount point'
        sudo mkdir -p -- "$mount_point"
        step='mount ext4 volume'
        sudo mount -- "$loop_device" "$mount_point"
        mounted=1
        step='identify runner uid'
        owner_uid=$(id -u)
        step='identify runner gid'
        owner_gid=$(id -g)
        step='give volume to runner'
        sudo chown "$owner_uid:$owner_gid" "$mount_point"
        step='create temporary directory'
        mkdir -- "$mount_point/tmp"
        step='report mounted volume'
        findmnt -no SOURCE,FSTYPE,OPTIONS -T "$mount_point/tmp"
        step='report write cache'
        printf 'queue/write_cache: '
        cat -- "$cache_path"
        step='report fua (diagnostic only)'
        printf 'queue/fua (diagnostic only): '
        cat -- "$fua_path"
        step='export TMPDIR'
        if [[ -v GITHUB_ENV ]]; then
            printf 'TMPDIR=%s/tmp\n' "$mount_point" >> "$GITHUB_ENV"
        else
            printf 'TMPDIR=%s/tmp\n' "$mount_point"
        fi
        ;;
    down)
        step='verify image for down'
        [[ -f $image && ! -L $image ]] || fail "expected a regular image: $image"
        step='identify mounted source for down'
        loop_device=$(findmnt --noheadings --raw --output SOURCE --mountpoint "$mount_point")
        [[ $loop_device =~ ^/dev/loop[0-9]+$ ]] || fail "mount source is not a loop device: $loop_device"
        step='verify loop backing image'
        associated_devices=$(sudo losetup --list --associated "$image" --noheadings --output NAME)
        matched=0
        while IFS= read -r candidate; do
            if [[ $candidate == "$loop_device" ]]; then
                matched=1
            fi
        done <<< "$associated_devices"
        (( matched )) || fail "mount source $loop_device is not backed by $image"
        step='unmount volume'
        sudo umount -- "$mount_point"
        step='detach loop device'
        sudo losetup --detach "$loop_device"
        step='remove image'
        rm -- "$image"
        ;;
esac
