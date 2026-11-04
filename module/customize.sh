#!/system/bin/sh
# Magisk/KernelSU 模块安装脚本。
# 注意：本文件由安装器 source 执行，不能使用 set -e/set -u，避免污染调用方环境。
SKIPUNZIP=0

# 安装中的模块目录；旧版本模块目录用于迁移用户配置
OLD_MODULE_DIR="/data/adb/modules/AppOpt"

check_magisk_version() {
	ui_print "- Magisk version: $MAGISK_VER_CODE"
	ui_print "- Module version: $(grep_prop version "${TMPDIR}/module.prop")"
	ui_print "- Module versionCode: $(grep_prop versionCode "${TMPDIR}/module.prop")"
	ui_print "********************************************"
	ui_print "- $(grep_prop description "${TMPDIR}/module.prop")"
	if [ "$MAGISK_VER_CODE" -lt 20400 ]; then
		ui_print "********************************************"
		ui_print "! 请安装 Magisk v20.4+ (20400+)"
		abort    "********************************************"
	fi
}

check_required_files() {
	REQUIRED_FILE_LIST="/sys/devices/system/cpu/present /proc/loadavg"
	for REQUIRED_FILE in $REQUIRED_FILE_LIST; do
		if [ ! -e "$REQUIRED_FILE" ]; then
			ui_print "********************************************"
			ui_print "! $REQUIRED_FILE 文件不存在"
			ui_print "! 请联系模块作者"
			abort    "********************************************"
		fi
	done
}

extract_bin() {
	ui_print "********************************************"
	case "$ARCH" in
		arm64) ARCH_DIR="arm64-v8a" ;;
		x64)   ARCH_DIR="x86_64" ;;
		*)     abort "! Unsupported platform: $ARCH" ;;
	esac
	ui_print "- Device platform: $ARCH"

	for BIN_FILE in "$MODPATH/bin/$ARCH_DIR"/*; do
		[ -f "$BIN_FILE" ] || continue
		cp -f "$BIN_FILE" "$MODPATH/"
	done
	rm -rf "$MODPATH/bin"

	[ -f "$MODPATH/AppOpt" ] && chmod a+x "$MODPATH/AppOpt"
	if ! "$MODPATH/AppOpt" -v; then
		abort "! 主程序验证失败，请检查模块zip文件是否损坏"
	fi
}

# 把 "0 1 2 3" 这类 CPU 列表格式化为 "0-3"，空输入回退到全部 CPU
format_cpu_ranges() {
	INPUT_CPUS="$1"
	if [ -z "$(printf '%s' "$INPUT_CPUS" | tr -d ' \t')" ]; then
		cat /sys/devices/system/cpu/present
		return
	fi
	printf '%s' "$INPUT_CPUS" | awk '{
		n = split($0, arr, /[[:space:]]+/)
		j = 0
		for (i = 1; i <= n; i++) {
			if (arr[i] != "" && !seen[arr[i]]++)
				nums[++j] = arr[i] + 0
		}
		n = j
		if (!n) exit
		for (i = 1; i < n; i++) {
			min = i
			for (j = i + 1; j <= n; j++)
				if (nums[j] < nums[min]) min = j
			if (min != i) {
				t = nums[i]
				nums[i] = nums[min]
				nums[min] = t
			}
		}
		start = last = nums[1]
		for (i = 2; i <= n; i++) {
			if (nums[i] == last + 1) {
				last = nums[i]
				continue
			}
			printf "%s%s", sep, (start == last ? start : start "-" last)
			sep = ","
			start = last = nums[i]
		}
		printf "%s", sep
		printf (start == last ? start : start "-" last)
	}'
}

# 按 cpufreq 策略分组，频率从低到高；与主程序 cpuset.rs::assign_groups 的分档保持一致
detect_core_groups() {
	SORTED_GROUPS=$(
		for POLICY in /sys/devices/system/cpu/cpufreq/policy*; do
			[ -d "$POLICY" ] || continue
			POLICY_CPUS=$(cat "$POLICY/related_cpus" 2>/dev/null)
			POLICY_FREQ=$(cat "$POLICY/cpuinfo_max_freq" 2>/dev/null)
			[ -z "$POLICY_CPUS" ] || [ -z "$POLICY_FREQ" ] && continue
			echo "$POLICY_FREQ:$POLICY_CPUS"
		done | sort -n -t: -k1,1 | awk -F: '
			$1 == prev { cores = cores " " $2; next }
			prev != "" { print prev ":" cores; cores = "" }
			{ prev = $1; cores = $2 }
			END { if (prev != "") print prev ":" cores }'
	)
	eval "$(printf '%s' "$SORTED_GROUPS" | awk -F: '
		function join(s, v) { return (s == "" ? "" : s " ") v }
		BEGIN {
			e_core = ""; m_core = ""; p_core = ""; hp_core = ""
			e_core_freq = 0; m_core_freq = 0; p_core_freq = 0; hp_core_freq = 0
			total_groups = 0
		}
		{
			freq_arr[NR] = $1
			cpus_arr[NR] = $2
			total_groups = NR
		}
		END {
			if (total_groups == 0) {
				print "e_core=\"\"; m_core=\"\"; p_core=\"\"; hp_core=\"\"; e_core_freq=0; m_core_freq=0; p_core_freq=0; hp_core_freq=0; total_groups=0;"
				exit
			}
			e_core = cpus_arr[1]
			e_core_freq = freq_arr[1]
			if (total_groups >= 2) {
				hp_core = cpus_arr[total_groups]
				hp_core_freq = freq_arr[total_groups]
			}
			for (i = 2; i < total_groups; i++) {
				if (total_groups >= 4 && i == 2) {
					m_core = join(m_core, cpus_arr[i])
					if (freq_arr[i] > m_core_freq) m_core_freq = freq_arr[i]
				} else {
					p_core = join(p_core, cpus_arr[i])
					if (freq_arr[i] > p_core_freq) p_core_freq = freq_arr[i]
				}
			}
			printf "e_core=\"%s\"; m_core=\"%s\"; p_core=\"%s\"; hp_core=\"%s\"; ", e_core, m_core, p_core, hp_core
			printf "e_core_freq=%d; m_core_freq=%d; p_core_freq=%d; hp_core_freq=%d; total_groups=%d;", e_core_freq, m_core_freq, p_core_freq, hp_core_freq, total_groups
		}')"
}

module_instructions() {
	ui_print "********************************************"
	ui_print "线程规则配置文件路径为："
	ui_print "/data/adb/modules/AppOpt/applist.conf"
	ui_print "------------------------------------------"
	ui_print "修改与添加规则无需重启，即时生效"
	ui_print "********************************************"
	CORES_SPEC=$(
		for POLICY_CPUS in /sys/devices/system/cpu/cpufreq/*/related_cpus; do
			[ -f "$POLICY_CPUS" ] && cat "$POLICY_CPUS" | wc -w
		done | paste -sd+
	)
	E_FMT="$(format_cpu_ranges "$e_core")"
	M_FMT="$(format_cpu_ranges "$m_core")"
	P_FMT="$(format_cpu_ranges "$p_core")"
	H_FMT="$(format_cpu_ranges "$hp_core")"
	ALL_CORE="$(cat /sys/devices/system/cpu/present)"
	ui_print "当前$(getprop ro.soc.model)设备为$(nproc)核CPU，规格是：$CORES_SPEC"
	ui_print "可用CPU范围：$ALL_CORE"
	ui_print "------------------------------------------"
	[ -n "$E_FMT" ] && echo "$E_FMT 为能效小核，频率最高$((e_core_freq/1000)) MHz"
	[ "$total_groups" -ge 4 ] && [ -n "$M_FMT" ] && echo "$M_FMT 为低频中核，频率最高$((m_core_freq/1000)) MHz"
	[ "$total_groups" -ge 3 ] && [ -n "$P_FMT" ] && echo "$P_FMT 为高频中核，频率最高$((p_core_freq/1000)) MHz"
	[ "$total_groups" -ge 2 ] && [ -n "$H_FMT" ] && echo "$H_FMT 为大核，频率最高$((hp_core_freq/1000)) MHz"
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

# 旧配置优先：保留用户已有规则，包内示例另存为 applist.conf.bak
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

check_magisk_version
check_required_files
detect_core_groups
copy_default_rules
extract_bin
module_instructions
set_perm_recursive "$MODPATH" 0 0 0755 0644 u:object_r:magisk_file:s0
set_perm "$MODPATH/AppOpt" 0 2000 0755 u:object_r:magisk_file:s0
set_perm "$MODPATH/AppOpt-ebpf" 0 2000 0755 u:object_r:magisk_file:s0
set_perm "$MODPATH/service.sh" 0 2000 0755 u:object_r:magisk_file:s0
