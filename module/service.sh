#!/system/bin/sh

MODDIR=${0%/*}
[ -d "$MODDIR" ] || MODDIR=.

log_msg() { echo "AppOpt/service: $*"; }

wait_boot_completed() {
	local i=30
	while [ "$i" -gt 0 ]; do
		[ "$(getprop sys.boot_completed)" = "1" ] && return 0
		i=$((i - 1))
		sleep 6
	done
	log_msg "等待 sys.boot_completed 超时，继续启动"
	return 1
}

start_daemon() {
	if [ ! -x "$MODDIR/AppOpt" ]; then
		log_msg "未找到可执行的 AppOpt，跳过启动"
		return 1
	fi
	cd "$MODDIR" || return 1
	nohup "$MODDIR/AppOpt" -w >/dev/null 2>&1 &
	log_msg "已拉起 AppOpt（pid $!）"
}

relax_core_ctl() {
	for MAX_CPUS in /sys/devices/system/cpu/cpu*/core_ctl/max_cpus; do
		[ -e "$MAX_CPUS" ] || continue
		MIN_CPUS="${MAX_CPUS%/*}/min_cpus"
		[ -e "$MIN_CPUS" ] || continue
		MAX_VALUE=$(cat "$MAX_CPUS" 2>/dev/null)
		MIN_VALUE=$(cat "$MIN_CPUS" 2>/dev/null)
		[ -n "$MAX_VALUE" ] && [ "$MAX_VALUE" = "$MIN_VALUE" ] && continue
		chmod a+w "$MIN_CPUS" 2>/dev/null && echo "$MAX_VALUE" > "$MIN_CPUS"
		chmod a-w "$MIN_CPUS" 2>/dev/null
	done
}

wait_boot_completed
start_daemon
relax_core_ctl

# [ -n "$(getprop persist.sys.oiface.enable)" ] && setprop persist.sys.oiface.enable 0

# pm disable-user com.xiaomi.joyose; pm clear com.xiaomi.joyose
