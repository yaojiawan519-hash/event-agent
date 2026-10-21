/*
 * ea_ecat.c — motor-ecat 的 SOEM 薄封装实现。
 *
 * 移植自 LinuxWasm 的 runtime/src/hal/hal_ecat.c（已验证基线），
 * 剥掉了 WAMR 符号注册与内置 PDO pump 线程：
 *   - PDO pump 改由 Rust 的 RT 线程驱动（保证与总线 I/O 彻底分离）；
 *   - 不配置 Distributed Clock（原路径也没配，保持一致）。
 *
 * 注意：本文件**不做线程同步**——设计上只有一个 RT 线程调用
 * ea_ecat_pdo_cycle()，且初始化在启动阶段完成，此后无并发访问。
 */

#include "ea_ecat.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "soem/soem.h"

/* 2 个从站的实际 IOmap 约 98 B；留足余量。 */
#define EA_IOMAP_SIZE 4096

static ecx_contextt *g_ctx = NULL;
static uint8_t       g_iomap[EA_IOMAP_SIZE];
static int           g_slave_count = 0;
static int           g_iomap_bytes = 0;

int ea_ecat_init(const char *ifname)
{
    if (g_ctx) return g_slave_count; /* already initialised */

    if (!ifname || !*ifname) return EA_ECAT_ERR_ARG;

    g_ctx = (ecx_contextt *)calloc(1, sizeof(ecx_contextt));
    if (!g_ctx) return EA_ECAT_ERR_INIT;

    if (!ecx_init(g_ctx, ifname)) {
        fprintf(stderr, "ea_ecat: ecx_init(%s) failed — "
                        "check interface name and CAP_NET_RAW\n", ifname);
        free(g_ctx);
        g_ctx = NULL;
        return EA_ECAT_ERR_INIT;
    }

    if (ecx_config_init(g_ctx) <= 0) {
        fprintf(stderr, "ea_ecat: no slaves found on %s\n", ifname);
        free(g_ctx);
        g_ctx = NULL;
        return EA_ECAT_ERR_NOSLAVE;
    }
    g_slave_count = g_ctx->slavecount;

    memset(g_iomap, 0, sizeof(g_iomap));
    g_iomap_bytes = ecx_config_map_group(g_ctx, g_iomap, 0);
    if (g_iomap_bytes > (int)sizeof(g_iomap)) {
        fprintf(stderr, "ea_ecat: IOmap too small (need %d, have %d)\n",
                g_iomap_bytes, (int)sizeof(g_iomap));
        free(g_ctx);
        g_ctx = NULL;
        g_slave_count = 0;
        return EA_ECAT_ERR_IOMAP;
    }

    printf("ea_ecat: master on %s, %d slave(s), IOmap %d B\n",
           ifname, g_slave_count, g_iomap_bytes);
    fflush(stdout);
    return g_slave_count;
}

int ea_ecat_to_op(int slave)
{
    if (!g_ctx) return EA_ECAT_ERR_STATE;
    if (slave < 1 || slave > g_slave_count) return EA_ECAT_ERR_ARG;

    /* PRE_OP → SAFE_OP */
    g_ctx->slavelist[slave].state = EC_STATE_SAFE_OP;
    ecx_writestate(g_ctx, slave);
    if (ecx_statecheck(g_ctx, slave, EC_STATE_SAFE_OP, EC_TIMEOUTSTATE) != EC_STATE_SAFE_OP) {
        fprintf(stderr, "ea_ecat: slave %d failed to reach SAFE_OP\n", slave);
        return EA_ECAT_ERR_STATE;
    }

    /* SAFE_OP → OP：先跑一拍 PDO，再请求 OP（顺序不能反）。 */
    ecx_send_processdata(g_ctx);
    ecx_receive_processdata(g_ctx, EC_TIMEOUTRET);

    g_ctx->slavelist[slave].state = EC_STATE_OPERATIONAL;
    ecx_writestate(g_ctx, slave);

    /* 用 PDO 循环推进状态机（SOEM 要求周期收发才能完成 SAFE_OP→OP）。 */
    int retries = 40; /* 40 × 10 ms = 400 ms */
    for (int i = 0; i < retries; i++) {
        ecx_send_processdata(g_ctx);
        ecx_receive_processdata(g_ctx, EC_TIMEOUTRET);
        int st = ecx_statecheck(g_ctx, slave, EC_STATE_OPERATIONAL, 5000);
        if (st == EC_STATE_OPERATIONAL) {
            printf("ea_ecat: slave %d in OP\n", slave);
            fflush(stdout);
            return 0;
        }
        usleep(10000);
    }

    fprintf(stderr, "ea_ecat: slave %d failed to reach OP\n", slave);
    return EA_ECAT_ERR_STATE;
}

void ea_ecat_shutdown(void)
{
    if (!g_ctx) return;

    for (int i = 1; i <= g_slave_count; i++) {
        g_ctx->slavelist[i].state = EC_STATE_INIT;
        ecx_writestate(g_ctx, i);
    }
    ecx_send_processdata(g_ctx);
    usleep(10000);
    ecx_receive_processdata(g_ctx, EC_TIMEOUTRET);

    free(g_ctx);
    g_ctx = NULL;
    g_slave_count = 0;
    g_iomap_bytes = 0;
}

int ea_ecat_slave(int idx, ea_ecat_slave_t *out)
{
    if (!g_ctx || !out) return EA_ECAT_ERR_ARG;
    if (idx < 0 || idx >= g_slave_count) return EA_ECAT_ERR_ARG;

    ec_slavet *s = &g_ctx->slavelist[idx + 1]; /* SOEM 的 slavelist 从 1 开始 */
    out->outputs      = s->outputs;
    out->inputs       = s->inputs;
    out->output_bytes = s->Obits / 8;
    out->input_bytes  = s->Ibits / 8;
    out->vendor_id    = s->eep_man;
    out->product_code = s->eep_id;
    out->al_state     = s->state;
    return 0;
}

int ea_ecat_pdo_cycle(void)
{
    if (!g_ctx) return EA_ECAT_ERR_STATE;
    ecx_send_processdata(g_ctx);
    return ecx_receive_processdata(g_ctx, EC_TIMEOUTRET);
}
