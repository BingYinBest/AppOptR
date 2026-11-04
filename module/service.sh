wait_sys_boot_completed() {
	local i=30
	until [ "$(getprop sys.boot_completed)" == "1" ] || [ $i -le 0 ]; do
		i=$((i-1))
		sleep 6
	done
}
wait_sys_boot_completed
MODDIR=${0%/*}
cd "$MODDIR"

nohup "$MODDIR/AppOpt" -w >/dev/null 2>&1 &

for MAX_CPUS in /sys/devices/system/cpu/cpu*/core_ctl/max_cpus; do
	if [ -e "$MAX_CPUS" ] && [ "$(cat "$MAX_CPUS")" != "$(cat "${MAX_CPUS%/*}/min_cpus")" ]; then
		chmod a+w "${MAX_CPUS%/*}/min_cpus"
		echo "$(cat "$MAX_CPUS")" > "${MAX_CPUS%/*}/min_cpus"
		chmod a-w "${MAX_CPUS%/*}/min_cpus"
	fi
done

# 如需暂停绿厂oiface，将下面 setprop 行前的 # 删掉；恢复则将 0 改成 1
# [ -n "$(getprop persist.sys.oiface.enable)" ] && setprop persist.sys.oiface.enable 0

# 如需禁用米系机型joyose，将下面 pm 命令前的 # 删掉
# pm disable-user com.xiaomi.joyose; pm clear com.xiaomi.joyose