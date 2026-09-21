#!/usr/bin/env bash
set -euo pipefail

if [[ ${EUID:-$(id -u)} -ne 0 ]]; then
    exec sudo -E -- "$0" "$@"
fi

# root-agent/system services may provide a 1024 soft descriptor limit even
# though the hard limit is higher. Raise only this helper process; do not
# modify the host-wide fs.file-max sysctl.
ulimit -n 65536 2>/dev/null || true

# Resolve the checkout through git rather than counting directories up from this
# script: the path arithmetic silently produced the wrong root whenever a script
# moved. The fallback covers a non-git export, and `sudo` re-execs, where git
# refuses a checkout owned by another user.
repo_dir=$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null) ||
    repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
iw_bin=${IW_BIN:-iw}
namespace="unifi-hwsim-${USER:-root}-$$"
socket_path="${TMPDIR:-/tmp}/unifi-wifi-compat-${USER:-root}-$$.sock"
control_path="${TMPDIR:-/tmp}/unifi-wifi-control-${USER:-root}-$$.sock"
backend_log="${TMPDIR:-/tmp}/unifi-hwsim-${USER:-root}-$$.log"
run_qemu=false
cleanup_only=false
instances=()
instance_sockets=()
socket_dir=${TMPDIR:-/tmp}
instances_table="${TMPDIR:-/tmp}/unifi-hwsim-instances-${USER:-root}-$$.json"
module_loaded_by_script=false
radios_in_namespace=false
host_fallback=false

usage() {
    echo "usage: sudo $0 [--socket PATH] [--control PATH] [--instance NAME]... [--socket-dir DIR] [--run-qemu|--cleanup]"
    echo "  --socket      fixed frame socket path (default: a PID-derived path in TMPDIR)"
    echo "  --control     fixed medium control socket path"
    echo "  --instance    serve this named console instance (repeatable)"
    echo "  --socket-dir  directory for per-instance sockets (default: TMPDIR)"
    echo "  --run-qemu  launch task u6plus after the isolated backend is ready"
    echo "  --cleanup   remove abandoned helper namespaces, hwsim radios, and sockets"
}
while (($#)); do
    case $1 in
        # A console instance names fixed socket paths in its YAML (wifi.socket
        # and wifi.control), so allow the operator to pin them here instead of
        # using the PID-derived defaults above.
        --socket) socket_path=${2:?--socket needs a path}; shift 2 ;;
        --control) control_path=${2:?--control needs a path}; shift 2 ;;
        # One daemon can serve several console sessions; each named instance
        # gets its own frame socket and its own pair of hwsim radios, and they
        # all share this process and one control socket.
        --instance) instances+=("${2:?--instance needs a name}"); shift 2 ;;
        --socket-dir) socket_dir=${2:?--socket-dir needs a directory}; shift 2 ;;
        --run-qemu) run_qemu=true; shift ;;
        --cleanup) cleanup_only=true; shift ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
done

module_loaded() {
    [[ -e /sys/module/mac80211_hwsim ]] ||
        lsmod 2>/dev/null | awk '{print $1}' | rg -qxF mac80211_hwsim
}

cleanup_stale_state() {
    while read -r stale_namespace; do
        [[ -n $stale_namespace ]] || continue
        [[ $stale_namespace == unifi-hwsim-* ]] || continue
        echo "removing abandoned hwsim namespace: $stale_namespace" >&2
        ip netns del "$stale_namespace" 2>/dev/null || true
    done < <(ip netns list | awk '$1 ~ /^unifi-hwsim-/ {print $1}')

    if module_loaded; then
        echo "removing mac80211_hwsim module and its radios" >&2
        if ! modprobe -r mac80211_hwsim; then
            echo "could not unload mac80211_hwsim; another process may be using it" >&2
            return 1
        fi
    fi

    find /tmp -maxdepth 1 -type s -name 'unifi-wifi-compat-*.sock' -print -delete 2>/dev/null || true
}

cleanup() {
    if [[ -n ${netns_holder_pid:-} ]] && kill -0 "$netns_holder_pid" 2>/dev/null; then
        kill "$netns_holder_pid" 2>/dev/null || true
        wait "$netns_holder_pid" 2>/dev/null || true
    fi
    if [[ -n ${netns_exec_pid:-} ]] && kill -0 "$netns_exec_pid" 2>/dev/null; then
        kill "$netns_exec_pid" 2>/dev/null || true
        wait "$netns_exec_pid" 2>/dev/null || true
    fi
    if [[ -n ${backend_pid:-} ]] && kill -0 "$backend_pid" 2>/dev/null; then
        kill "$backend_pid" 2>/dev/null || true
        wait "$backend_pid" 2>/dev/null || true
    fi
    if [[ -n ${control_chmod_pid:-} ]] && kill -0 "$control_chmod_pid" 2>/dev/null; then
        kill "$control_chmod_pid" 2>/dev/null || true
        wait "$control_chmod_pid" 2>/dev/null || true
    fi
    if ip netns list | awk '{print $1}' | rg -qxF "$namespace"; then
        ip netns del "$namespace" 2>/dev/null || true
    fi
    if $module_loaded_by_script; then
        modprobe -r mac80211_hwsim 2>/dev/null || true
    fi
    rm -f -- "$socket_path" "$control_path" "$instances_table" "${instance_sockets[@]}"
}
trap cleanup EXIT INT TERM

if ! command -v "$iw_bin" >/dev/null 2>&1; then
    echo "missing iw; install wireless-tools (on NixOS: nix shell nixpkgs#iw)" >&2
    echo "then rerun this helper, or set IW_BIN=/path/to/iw" >&2
    exit 1
fi

if $cleanup_only; then
    cleanup_stale_state
    echo "hwsim cleanup complete"
    exit 0
fi

# Ctrl-C normally reaches cleanup(), but a terminal/session kill can leave the
# disposable namespace mounted. Remove only namespaces owned by this helper;
# never touch arbitrary user namespaces.
cleanup_stale_state

echo "hwsim: creating namespace $namespace" >&2
if ! ip netns add "$namespace" 2>/tmp/unifi-hwsim-netns-error; then
    echo "cannot create network namespace; run with a host that permits ip netns" >&2
    sed -n '1,2p' /tmp/unifi-hwsim-netns-error >&2 || true
    exit 1
fi

list_phys() {
    local path
    for path in /sys/class/ieee80211/phy[0-9]*; do
        [[ -e $path ]] || continue
        basename "$path"
    done | sort -V
}

is_hwsim_phy() {
    local phy=$1 device driver
    device=$(readlink -f "/sys/class/ieee80211/$phy/device" 2>/dev/null || true)
    driver=$(readlink -f "/sys/class/ieee80211/$phy/device/driver" 2>/dev/null || true)
    [[ ${driver##*/} == mac80211_hwsim ]] && return 0

    # hwsim PHYs are virtual devices. On several kernels their device node is
    # under /sys/devices/virtual/ieee80211 and has no driver symlink at all.
    [[ -e /sys/module/mac80211_hwsim ]] &&
        [[ $device == */devices/virtual/ieee80211/* ]] && return 0

    rg -qsi 'mac80211_hwsim' \
        "/sys/class/ieee80211/$phy/device/modalias" \
        "/sys/class/ieee80211/$phy/device/uevent" 2>/dev/null
}

run_iw() {
    local status=0
    echo "hwsim: iw $*" >&2
    "$iw_bin" "$@" || status=$?
    return "$status"
}

phy_in_namespace() {
    ip netns exec "$namespace" "$iw_bin" phy "$1" info >/dev/null 2>&1
}

# First try to create the radios directly in the disposable namespace. This
# avoids the kernel's fragile second- PHY netns move entirely. Some kernels
# still register module-created radios in init_net; those use the host
# hwsim fallback below because this kernel rejects moving the second PHY.
instance_count=${#instances[@]}
((instance_count)) || instance_count=1
radio_count=$((instance_count * 2))

echo "hwsim: loading $radio_count radios inside namespace" >&2
if ip netns exec "$namespace" modprobe mac80211_hwsim "radios=$radio_count" 2>/tmp/unifi-hwsim-modprobe-error; then
    module_loaded_by_script=true
    mapfile -t phys < <(
        ip netns exec "$namespace" "$iw_bin" phy 2>/dev/null |
            sed -n 's/^Wiphy \(phy[[:digit:]]\+\)$/\1/p'
    )
    if ((${#phys[@]} == radio_count)); then
        radios_in_namespace=true
    else
        echo "hwsim: kernel placed radios outside namespace; using host hwsim fallback" >&2
        modprobe -r mac80211_hwsim 2>/dev/null || true
        module_loaded_by_script=false
        host_fallback=true
    fi
else
    sed -n '1,2p' /tmp/unifi-hwsim-modprobe-error >&2 || true
    host_fallback=true
fi

# Kernel modules are global. Always reset this helper's hwsim module rather than reusing
# radios from an interrupted run; reuse can leave netlink objects attached to
# a dead namespace and make subsequent iw operations fail with ENFILE (-23).
if ! $radios_in_namespace && module_loaded; then
    echo "hwsim: resetting existing mac80211_hwsim radios" >&2
    if ! modprobe -r mac80211_hwsim; then
        echo "cannot remove mac80211_hwsim; stop its other user first" >&2
        exit 1
    fi
fi
if ! $radios_in_namespace; then
    echo "hwsim: loading mac80211_hwsim radios=$radio_count" >&2
    if ! modprobe mac80211_hwsim "radios=$radio_count"; then
    echo "cannot load mac80211_hwsim" >&2
    echo "check CAP_SYS_ADMIN/CAP_NET_ADMIN and the kernel module package" >&2
    exit 1
    fi
    module_loaded_by_script=true
    mapfile -t phys < <(
        while read -r phy; do
            is_hwsim_phy "$phy" && echo "$phy"
        done < <(list_phys)
    )
    host_fallback=true
fi
if ((${#phys[@]} != radio_count)); then
    echo "expected $radio_count hwsim PHYs, found ${#phys[@]}" >&2
        echo "detected PHYs: $(list_phys | paste -sd ' ' -)" >&2
        echo "mac80211_hwsim loaded: $(module_loaded && echo yes || echo no)" >&2
        exit 1
fi

# Keep a process in the target namespace so iw can resolve one stable netns
# inode by PID for every PHY move. This avoids repeated open-by-name lookups,
# which can return ENFILE (-23) on affected kernels after the first move.
if ! $radios_in_namespace && ! $host_fallback; then
    ip netns exec "$namespace" sleep 2147483647 &
    netns_exec_pid=$!
    netns_holder_pid=
    for _ in {1..20}; do
        mapfile -t namespace_pids < <(ip netns pids "$namespace" 2>/dev/null || true)
        if ((${#namespace_pids[@]})); then
            netns_holder_pid=${namespace_pids[0]}
            break
        fi
        sleep 0.05
    done

    if [[ -z $netns_holder_pid ]]; then
        echo "hwsim: namespace holder did not start" >&2
        exit 1
    fi

    moved_phys=()
    for phy in "${phys[@]}"; do
        echo "hwsim: iw phy $phy set netns $netns_holder_pid" >&2
        if "$iw_bin" phy "$phy" set netns "$netns_holder_pid" &&
            phy_in_namespace "$phy"; then
            moved_phys+=("$phy")
        else
            echo "hwsim: kernel refused the second PHY move; using host hwsim fallback" >&2
            for moved_phy in "${moved_phys[@]}"; do
                run_iw phy "$moved_phy" set netns "$$" || true
            done
            host_fallback=true
            break
        fi
    done
fi

if $host_fallback; then
    namespace_prefix=()
else
    namespace_prefix=(ip netns exec "$namespace")
fi

# mac80211_hwsim registers PHYs, but some kernels do not create managed WLAN
# netdevs until userspace asks for them. Create one per moved PHY.
for index in "${!phys[@]}"; do
    "${namespace_prefix[@]}" "$iw_bin" phy "${phys[index]}" \
        interface add "unifi-hwsim${index}" type managed
done

mapfile -t radio_macs < <(
    for index in "${!phys[@]}"; do
        "${namespace_prefix[@]}" ip -o link show dev "unifi-hwsim${index}"
    done |
        sed -n 's/.*link\/ether \([[:xdigit:]:]\{17\}\).*/\1/p'
)
if ((${#radio_macs[@]} != radio_count)); then
    echo "expected $radio_count hwsim radios, found ${#radio_macs[@]}" >&2
    ip netns exec "$namespace" ip -o link show >&2 || true
    exit 1
fi

"${namespace_prefix[@]}" python3 "$repo_dir/scripts/compat/hwsim_adapter.py" --probe >/dev/null

instance_sockets=()
if ((${#instances[@]})); then
    # One daemon, one radio pair per instance, one shared control socket.
    for index in "${!instances[@]}"; do
        instance_sockets+=("$socket_dir/hwsim-${instances[index]}.sock")
    done
    {
        echo -n "["
        for index in "${!instances[@]}"; do
            ((index)) && echo -n ","
            printf '{"name":"%s","socket":"%s","radios":{"band0":"%s","band1":"%s"}}' \
                "${instances[index]}" "${instance_sockets[index]}" \
                "${radio_macs[index * 2]}" "${radio_macs[index * 2 + 1]}"
        done
        echo -n "]"
    } >"$instances_table"
    "${namespace_prefix[@]}" python3 "$repo_dir/scripts/compat/hwsim_adapter.py" \
        --instances "$instances_table" \
        --control "$control_path" \
        --own-medium >"$backend_log" 2>&1 &
else
    instance_sockets+=("$socket_path")
    "${namespace_prefix[@]}" python3 "$repo_dir/scripts/compat/hwsim_adapter.py" \
        --socket "$socket_path" \
        --control "$control_path" \
        --radio "band0=${radio_macs[0]}" \
        --radio "band1=${radio_macs[1]}" \
        --own-medium >"$backend_log" 2>&1 &
fi
backend_pid=$!

for path in "${instance_sockets[@]}"; do
    for _ in {1..100}; do
        [[ -S "$path" ]] && break
        if ! kill -0 "$backend_pid" 2>/dev/null; then
            sed -n '1,120p' "$backend_log" >&2 || true
            exit 1
        fi
        sleep 0.05
    done
    [[ -S "$path" ]] || { echo "hwsim backend did not create $path" >&2; exit 1; }
    chmod 0666 "$path"
done
(for _ in {1..1200}; do
    if [[ -S "$control_path" ]]; then
        chmod 0666 "$control_path"
        exit 0
    fi
    kill -0 "$backend_pid" 2>/dev/null || exit 1
    sleep 0.05
done) &
control_chmod_pid=$!
echo "isolated namespace: $namespace"
if $host_fallback; then
    echo "warning: kernel cannot move two hwsim PHYs; backend is using the host hwsim namespace" >&2
fi
if ((${#instances[@]})); then
    for index in "${!instances[@]}"; do
        echo "instance ${instances[index]}: ${instance_sockets[index]}" \
             "band0=${radio_macs[index * 2]} band1=${radio_macs[index * 2 + 1]}"
    done
    echo "point each console instance at its socket with wifi.socket, and at"
    echo "$control_path with wifi.control plus wifi.instance: NAME"
else
    echo "hwsim radios: band0=${radio_macs[0]} band1=${radio_macs[1]}"
    echo "run in another terminal: MT7981_HWSIM_SOCKET=$socket_path task u6plus"
fi
echo "backend log: $backend_log"
echo "medium control: $control_path"
echo "medium control socket: $control_path"

if $run_qemu; then
    invoking_user=${SUDO_USER:-root}
    if [[ $invoking_user == root ]]; then
        MT7981_HWSIM_SOCKET="$socket_path" task u6plus
    else
        runuser -u "$invoking_user" -- env MT7981_HWSIM_SOCKET="$socket_path" task u6plus
    fi
else
    echo "press Ctrl-C to tear down the namespace"
    wait "$backend_pid"
fi
