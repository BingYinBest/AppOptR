#!/system/bin/sh
SKIPUNZIP=0

OLD_MODULE_DIR="/data/adb/modules/AppOpt"

e_core=""; e_core_freq=0
m_core=""; m_core_freq=0
p_core=""; p_core_freq=0
hp_core=""; hp_core_freq=0
total_groups=0

die() {
	ui_print "! $1"
	abort   "********************************************"
}

check_magisk_version() {
	ui_print "- Magisk version: ${MAGISK_VER_CODE:-0}"
	ui_print "- Module version: $(grep_prop version "${TMPDIR}/module.prop")"
	ui_print "- Module versionCode: $(grep_prop versionCode "${TMPDIR}/module.prop")"
	ui_print "********************************************"
	ui_print "- $(grep_prop description "${TMPDIR}/module.prop")"
	if [ "${MAGISK_VER_CODE:-0}" -lt 20400 ]; then
		die "请安装 Magisk v20.4+ (20400+)"
	fi
}

check_required_files() {
	for REQUIRED_FILE in /sys/devices/system/cpu/present /proc/loadavg; do
		if [ ! -e "$REQUIRED_FILE" ]; then
			ui_print "! $REQUIRED_FILE 文件不存在"
			die "请联系模块作者"
		fi
	done
}

resolve_arch() {
	case "$ARCH" in
		arm64) BIN_DIR="arm64-v8a" ;;
		x64)   BIN_DIR="x86_64" ;;
		*)     die "Unsupported platform: $ARCH" ;;
	esac
	ui_print "- Device platform: $ARCH"
}

install_binaries() {
	ui_print "********************************************"
	for BIN_FILE in "$MODPATH/bin/$BIN_DIR"/*; do
		[ -f "$BIN_FILE" ] || continue
		cp -f "$BIN_FILE" "$MODPATH/" || die "复制 $(basename "$BIN_FILE") 失败"
	done
	rm -rf "$MODPATH/bin"

	[ -f "$MODPATH/AppOpt" ] || die "包内缺少 AppOpt 主程序"
	chmod a+x "$MODPATH/AppOpt"
	[ -f "$MODPATH/AppOpt-ebpf" ] && chmod a+x "$MODPATH/AppOpt-ebpf"

	if ! "$MODPATH/AppOpt" -v; then
		die "主程序验证失败，请检查模块zip文件是否损坏"
	fi
}

format_cpu_ranges() {
	INPUT_CPUS="$1"
	if [ -z "$(printf '%s' "$INPUT_CPUS" | tr -d ' \t')" ]; then
		cat /sys/devices/system/cpu/present
		return
	fi
	printf '%s\n' "$INPUT_CPUS" | tr -s ' \t' '\n\n' | sed '/^$/d' | sort -n | uniq | awk '
		NR == 1 { start = last = $1; next }
		$1 == last + 1 { last = $1; next }
		{
			printf "%s%s", sep, (start == last ? start : start "-" last)
			sep = ","
			start = last = $1
		}
		END {
			if (NR) printf "%s%s", sep, (start == last ? start : start "-" last)
		}'
}

detect_core_groups() {
	POLICY_GROUPS=$(
		for POLICY in /sys/devices/system/cpu/cpufreq/policy*; do
			[ -d "$POLICY" ] || continue
			POLICY_CPUS=$(cat "$POLICY/related_cpus" 2>/dev/null)
			POLICY_FREQ=$(cat "$POLICY/cpuinfo_max_freq" 2>/dev/null)
			[ -z "$POLICY_CPUS" ] || [ -z "$POLICY_FREQ" ] && continue
			printf '%s:%s\n' "$POLICY_FREQ" "$POLICY_CPUS"
		done | sort -n -t: -k1,1 | awk -F: '
			$1 == prev { cores = cores " " $2; next }
			prev != "" { print prev ":" cores; cores = "" }
			{ prev = $1; cores = $2 }
			END { if (prev != "") print prev ":" cores }'
	)

	PARSED=$(
		printf '%s\n' "$POLICY_GROUPS" | awk -F: '
			function join(s, v) { return (s == "" ? "" : s " ") v }
			NF == 0 { next }
			{ freq[++n] = $1; cpus[n] = $2 }
			END {
				e_core = ""; m_core = ""; p_core = ""; hp_core = ""
				e_freq = 0; m_freq = 0; p_freq = 0; hp_freq = 0
				if (n == 0) {
					printf "|||||||0"
					exit
				}
				e_core = cpus[1]; e_freq = freq[1]
				if (n >= 2) { hp_core = cpus[n]; hp_freq = freq[n] }
				for (i = 2; i < n; i++) {
					if (n >= 4 && i == 2) {
						m_core = join(m_core, cpus[i])
						if (freq[i] > m_freq) m_freq = freq[i]
					} else {
						p_core = join(p_core, cpus[i])
						if (freq[i] > p_freq) p_freq = freq[i]
					}
				}
				printf "%s|%d|%s|%d|%s|%d|%s|%d|%d", \
					e_core, e_freq, m_core, m_freq, p_core, p_freq, hp_core, hp_freq, n
			}'
	)

	IFS='|' read -r e_core e_core_freq m_core m_core_freq p_core p_core_freq hp_core hp_core_freq total_groups <<EOF
$PARSED
EOF
}

module_instructions() {
	ui_print "********************************************"
	ui_print "线程规则配置文件路径为："
	ui_print "/data/adb/modules/AppOpt/applist.conf"
	ui_print "------------------------------------------"
	ui_print "修改与添加规则无需重启，即时生效"
	ui_print "********************************************"

	E_FMT="$(format_cpu_ranges "$e_core")"
	M_FMT="$(format_cpu_ranges "$m_core")"
	P_FMT="$(format_cpu_ranges "$p_core")"
	H_FMT="$(format_cpu_ranges "$hp_core")"
	ALL_CORE="$(cat /sys/devices/system/cpu/present)"

	ui_print "当前$(getprop ro.soc.model)设备为$(nproc)核CPU，规格是：$(nproc)"
	ui_print "可用CPU范围：$ALL_CORE"
	ui_print "------------------------------------------"
	[ -n "$E_FMT" ] && echo "$E_FMT 为能效小核，频率最高$((e_core_freq / 1000)) MHz"
	[ "$total_groups" -ge 4 ] && [ -n "$M_FMT" ] && echo "$M_FMT 为低频中核，频率最高$((m_core_freq / 1000)) MHz"
	[ "$total_groups" -ge 3 ] && [ -n "$P_FMT" ] && echo "$P_FMT 为高频中核，频率最高$((p_core_freq / 1000)) MHz"
	[ "$total_groups" -ge 2 ] && [ -n "$H_FMT" ] && echo "$H_FMT 为大核，频率最高$((hp_core_freq / 1000)) MHz"

	ui_print "------------------------------------------"
	ui_print "applist.conf 规则写法示例："
	ui_print "------------------------------------------"
	if [ -n "$ALL_CORE" ]; then
		ui_print "允许安卓 '图形显示组件' 调用所有核心："
		ui_print "surfaceflinger=$ALL_CORE"
		ui_print "------------------------------------------"
	fi
	if [ -n "$H_FMT" ]; then
		ui_print "单独将'图形显示组件'渲染引擎线程绑定到大核："
		ui_print "surfaceflinger{RenderEngine}=$H_FMT"
		ui_print "------------------------------------------"
	fi
	if [ -n "$E_FMT" ]; then
		ui_print "将 '微信' 主进程绑定能效小核："
		ui_print "com.tencent.mm=$E_FMT"
		ui_print "------------------------------------------"
		ui_print "将 '微信' 消息推送子进程绑定能效小核："
		ui_print "com.tencent.mm:push=$E_FMT"
		ui_print "------------------------------------------"
	fi
	if [ -n "$H_FMT" ]; then
		ui_print "将 '系统界面' 渲染线程绑定到大核："
		ui_print "com.android.systemui{RenderThread}=$H_FMT"
		ui_print "------------------------------------------"
	fi
	ui_print "更多规则使用说明请参考："
	ui_print "http://AppOpt.suto.top"
	ui_print "------------------------------------------"
	ui_print "Web 管理面板："
	ui_print "http://127.0.0.1:8889"
	ui_print "********************************************"
}

copy_default_rules() {
	[ "$OLD_MODULE_DIR" = "$MODPATH" ] && return 0

	if [ -f "$OLD_MODULE_DIR/applist.conf" ]; then
		mv -f "$MODPATH/applist.conf" "$MODPATH/applist.conf.bak" 2>/dev/null
		if ! cp -f "$OLD_MODULE_DIR/applist.conf" "$MODPATH/applist.conf"; then
			ui_print "! 旧规则配置复制失败，已保留包内示例配置"
		fi
	fi
	if [ -f "$OLD_MODULE_DIR/AppOpt.json" ]; then
		cp -f "$OLD_MODULE_DIR/AppOpt.json" "$MODPATH/AppOpt.json" 2>/dev/null
	fi
}

apply_permissions() {
	set_perm_recursive "$MODPATH" 0 0 0755 0644 u:object_r:magisk_file:s0
	for EXEC_FILE in AppOpt AppOpt-ebpf service.sh; do
		[ -f "$MODPATH/$EXEC_FILE" ] || continue
		set_perm "$MODPATH/$EXEC_FILE" 0 2000 0755 u:object_r:magisk_file:s0
	done
}

check_magisk_version
check_required_files
detect_core_groups
copy_default_rules
resolve_arch
install_binaries
module_instructions
apply_permissions
