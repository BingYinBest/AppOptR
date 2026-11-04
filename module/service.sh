#!/system/bin/sh
# 开机自启：拉起 AppOpt 守护进程，并放开各 CPU 的小核调度限制
# 注意：本脚本由 Magisk/KernelSU 在 late_start 阶段执行，失败不应中断启动流程

# $0 可能是不带路径的相对路径，需回退到当前目录
MODDIR=${0%/*}
[ "$MODDIR" = "$0" ] && MODDIR=.

wait_sys_boot_completed() {
	local i=30
	until [ "$(getprop sys.boot_completed)" = "1" ] || [ "$i" -le 0 ]; do
		i=$((i - 1))
		sleep 6
	done
}

wait_sys_boot_completed

cd "$MODDIR" || exit 1
nohup "$MODDIR/AppOpt" -w >/dev/null 2>&1 &

# 放开 core_ctl 限制：min_cpus 拉到 max_cpus，避免小核被内核长期压低
for MAX_CPUS in /sys/devices/system/cpu/cpu*/core_ctl/max_cpus; do
	[ -e "$MAX_CPUS" ] || continue
	MIN_CPUS="${MAX_CPUS%/*}/min_cpus"
	[ -e "$MIN_CPUS" ] || continue
	MAX_VALUE=$(cat "$MAX_CPUS")
	MIN_VALUE=$(cat "$MIN_CPUS")
	[ "$MAX_VALUE" = "$MIN_VALUE" ] && continue
	chmod a+w "$MIN_CPUS" && echo "$MAX_VALUE" > "$MIN_CPUS"
	chmod a-w "$MIN_CPUS"
done

# 如需暂停绿厂oiface，将下面 setprop 行前的 # 删掉；恢复则将 0 改成 1
# [ -n "$(getprop persist.sys.oiface.enable)" ] && setprop persist.sys.oiface.enable 0

# 如需禁用米系机型joyose，将下面 pm 命令前的 # 删掉
# pm disable-user com.xiaomi.joyose; pm clear com.xiaomi.joyose
