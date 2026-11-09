
use std::env;
use std::process;

pub struct CliOptions {
    pub config_file: Option<String>,
    pub interval: Option<u64>,
    pub cpuset_name: Option<String>,
    pub web: bool,
    pub web_port: Option<u16>,
}

fn print_help() {
    println!("Usage: AppOpt [OPTIONS]");
    println!("Options:");
    println!("  -c <config_file>   指定配置文件 (默认: ./applist.conf)");
    println!("  -s <interval>      设置检查间隔(秒) (必须>=1, 默认: 2)");
    println!("  -b <cpuset_name>   指定 BASE_CPUSET 目录名 (默认: AppOpt)");
    println!("  -w                 启用网页前端 (仅监听 127.0.0.1)");
    println!("  -p <port>          网页前端端口 (1024-65535, 默认: 设置值或 8889)");
    println!("  -v                 显示程序版本");
    println!("  -h                 显示帮助信息");
}

fn arg_value(args: &[String], index: &mut usize, flag: &str) -> String {
    *index += 1;
    match args.get(*index) {
        Some(value) => value.clone(),
        None => {
            eprintln!("错误: {} 需要指定参数值", flag);
            process::exit(1);
        }
    }
}

pub fn parse_args(args: &[String]) -> CliOptions {
    let mut opts = CliOptions {
        config_file: None,
        interval: None,
        cpuset_name: None,
        web: false,
        web_port: None,
    };
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-c" => {
                let path = arg_value(args, &mut i, "-c");
                println!("配置文件: {}", path);
                opts.config_file = Some(path);
            }
            "-s" => {
                let raw = arg_value(args, &mut i, "-s");
                match raw.parse::<u64>() {
                    Ok(value) if value >= 1 => {
                        println!("检查间隔: {} 秒", value);
                        opts.interval = Some(value);
                    }
                    _ => {
                        eprintln!("无效的时间间隔: {}", raw);
                        eprintln!("间隔必须是 >=1 的整数");
                        process::exit(1);
                    }
                }
            }
            "-w" => opts.web = true,
            "-p" => {
                let raw = arg_value(args, &mut i, "-p");
                match raw.parse::<u16>() {
                    Ok(value) if (1024..=65535).contains(&value) => {
                        println!("Web 端口: {}", value);
                        opts.web_port = Some(value);
                    }
                    _ => {
                        eprintln!("无效的 Web 端口: {}", raw);
                        eprintln!("端口必须是 1024-65535 之间的整数");
                        process::exit(1);
                    }
                }
            }
            "-b" => {
                let name = arg_value(args, &mut i, "-b");
                if name.is_empty() || name.contains('/') {
                    eprintln!("无效的 cpuset 目录名: {}", name);
                    eprintln!("目录名不能为空或包含路径分隔符");
                    process::exit(1);
                }
                println!("cpuset 目录名: {}", name);
                opts.cpuset_name = Some(name);
            }
            "-v" => {
                let ebpf = crate::engine::ebpf::ebpf_probe();
                println!(
                    "AppOpt 版本 {}{}",
                    env!("CARGO_PKG_VERSION"),
                    if ebpf { " eBPF" } else { "" }
                );
                process::exit(0);
            }
            "-h" => {
                print_help();
                process::exit(0);
            }
            other => {
                eprintln!("未知选项: {}", other);
                print_help();
                process::exit(1);
            }
        }
        i += 1;
    }
    opts
}
