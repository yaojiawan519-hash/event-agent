/*
 * ea_ecat.h — motor-ecat 的 SOEM 薄封装。
 *
 * 只暴露「初始化 / 状态机 / 一次 PDO 收发 / 从站 PDO 指针」，
 * PDO 的字节级打包解包留在 Rust 侧（布局见 docs/DESIGN.md §8.8）。
 */

#ifndef EA_ECAT_H
#define EA_ECAT_H

#include <stdint.h>

/* 错误码（ea_ecat_init / ea_ecat_to_op 返回负值时） */
#define EA_ECAT_ERR_INIT      (-1) /* ecx_init 失败：接口名或 CAP_NET_RAW */
#define EA_ECAT_ERR_NOSLAVE   (-2) /* 总线上没有从站 */
#define EA_ECAT_ERR_IOMAP     (-3) /* IOmap 太小 */
#define EA_ECAT_ERR_STATE     (-4) /* 状态机切换超时 */
#define EA_ECAT_ERR_ARG       (-5) /* 参数越界 */

typedef struct {
    uint8_t *outputs;      /* RxPDO 起始（主站写） */
    uint8_t *inputs;       /* TxPDO 起始（主站读） */
    int      output_bytes; /* RxPDO 字节数 */
    int      input_bytes;  /* TxPDO 字节数 */
    uint32_t vendor_id;
    uint32_t product_code;
    uint16_t al_state;     /* 当前 AL 状态 */
} ea_ecat_slave_t;

/* 打开 ifname 上的 EtherCAT 主站、扫描从站、映射 PDO。
 * 返回从站数（>0），失败返回上面的负错误码。 */
int ea_ecat_init(const char *ifname);

/* 把从站（1-based）推到 OP。返回 0 成功，负值为错误码。 */
int ea_ecat_to_op(int slave);

/* 从站的 AL 状态（SOEM 缓存值，不产生总线流量）。idx 为 0-based；失败返回负错误码。
 * 2=INIT 4=SAFE_OP 8=OP */
int ea_ecat_al_state(int idx);

/* 从站掉出 OP（常见于主站异常退出后驱动器 SM 看门狗触发）后重新请求 OP。
 * 内部会重新走 SAFE_OP→OP 并跑若干拍 PDO，**会阻塞几十到几百毫秒**，
 * 只应在故障恢复路径里调用。slave 为 1-based。返回 0 成功。 */
int ea_ecat_reop(int slave);

/* 全部从站回到 INIT 并释放主站。 */
void ea_ecat_shutdown(void);

/* idx 为 0-based；成功返回 0。 */
int ea_ecat_slave(int idx, ea_ecat_slave_t *out);

/* 一次 PDO 收发：send + receive。返回工作计数 wkc，负值为错误码。 */
int ea_ecat_pdo_cycle(void);

/* CoE SDO 读（须在 PRE_OP；*size 传入缓冲区大小、传出实际长度）。
 * 返回 0 成功，负值为错误码。 */
int ea_ecat_sdo_read(int slave, uint16_t index, uint8_t sub, void *buf, int *size);

/* CoE SDO 写。返回 0 成功，负值为错误码。 */
int ea_ecat_sdo_write(int slave, uint16_t index, uint8_t sub, const void *buf, int size);

#endif /* EA_ECAT_H */
