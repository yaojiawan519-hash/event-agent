//! 编译 vendored SOEM（2.0.0）+ `csrc/ea_ecat.c`。
//!
//! 只在 Linux 目标上编译：EtherCAT 需要 AF_PACKET raw socket 与 Linux 网卡驱动，
//! macOS 开发机上退化为 mock 后端。

fn main() {
    println!("cargo:rerun-if-changed=vendor/soem");
    println!("cargo:rerun-if-changed=csrc/ea_ecat.c");
    println!("cargo:rerun-if-changed=csrc/ea_ecat.h");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }

    let mut build = cc::Build::new();
    build
        .include("vendor/soem/include")
        .include("vendor/soem/osal")
        .include("vendor/soem/osal/linux")
        .include("vendor/soem/oshw/linux")
        .include("csrc")
        // SOEM 自带大量历史代码，关掉告警避免噪声淹没真正的编译错误。
        .warnings(false)
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-but-set-variable");

    for f in [
        "vendor/soem/src/ec_base.c",
        "vendor/soem/src/ec_coe.c",
        "vendor/soem/src/ec_config.c",
        "vendor/soem/src/ec_dc.c",
        "vendor/soem/src/ec_eoe.c",
        "vendor/soem/src/ec_foe.c",
        "vendor/soem/src/ec_main.c",
        "vendor/soem/src/ec_print.c",
        "vendor/soem/src/ec_soe.c",
        "vendor/soem/osal/linux/osal.c",
        "vendor/soem/oshw/linux/nicdrv.c",
        "vendor/soem/oshw/linux/oshw.c",
        "csrc/ea_ecat.c",
    ] {
        build.file(f);
    }

    build.compile("eaecat");
}
