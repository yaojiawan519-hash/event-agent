/*
 * This software is dual-licensed under GPLv3 and a commercial
 * license. See the file LICENSE.md distributed with this software for
 * full license information.
 */

/** \file
 * \brief
 * Build options for SOEM
 *
 * NOTE: 由 SOEM 2.0.0 的 ec_options.h.in 按 CMakeLists.txt 默认值静态生成，
 *       不依赖 CMake configure。改成其它值时请同步 build.rs 的编译宏。
 */

#ifndef _ec_options_
#define _ec_options_

#ifdef __cplusplus
extern "C" {
#endif

/* Max sizes */

/** standard frame buffer size in bytes */
#define EC_BUFSIZE (EC_MAXECATFRAME)

/** number of frame buffers per channel (tx, rx1 rx2) */
#define EC_MAXBUF (16)

/** size of EEPROM bitmap cache */
#define EC_MAXEEPBITMAP (128)

/** size of EEPROM cache buffer */
#define EC_MAXEEPBUF (EC_MAXEEPBITMAP << 5)

/** default group size in 2^x */
#define EC_LOGGROUPOFFSET (16)

/** max. entries in EtherCAT error list */
#define EC_MAXELIST (64)

/** max. length of readable name in slavelist and Object Description List */
#define EC_MAXNAME (40)

/** max. number of slaves in array */
#define EC_MAXSLAVE (200)

/** max. number of groups */
#define EC_MAXGROUP (2)

/** max. number of IO segments per group */
#define EC_MAXIOSEGMENTS (64)

/** max. mailbox size */
#define EC_MAXMBX (1486)

/** number of mailboxes in pool */
#define EC_MBXPOOLSIZE (32)

/** max. eeprom PDO entries */
#define EC_MAXEEPDO (0x200)

/** max. SM used */
#define EC_MAXSM (8)

/** max. FMMU used */
#define EC_MAXFMMU (4)

/** max. adapter name length */
#define EC_MAXLEN_ADAPTERNAME (128)

/** define maximum number of concurrent threads in mapping */
#define EC_MAX_MAPT (1)

/** max entries in Object Description list */
#define EC_MAXODLIST (1024)

/** max entries in Object Entry list */
#define EC_MAXOELIST (256)

/** max. length of readable SoE name */
#define EC_SOE_MAXNAME (60)

/** max. number of SoE mappings */
#define EC_SOE_MAXMAPPING (64)

/* Timeouts and retries */

/** timeout value in us for tx frame to return to rx */
#define EC_TIMEOUTRET (2000)

/** timeout value in us for safe data transfer, max. triple retry */
#define EC_TIMEOUTRET3 (EC_TIMEOUTRET * 3)

/** timeout value in us for return "safe" variant (f.e. wireless) */
#define EC_TIMEOUTSAFE (20000)

/** timeout value in us for EEPROM access */
#define EC_TIMEOUTEEP (20000)

/** timeout value in us for tx mailbox cycle */
#define EC_TIMEOUTTXM (20000)

/** timeout value in us for rx mailbox cycle */
#define EC_TIMEOUTRXM (700000)

/** timeout value in us for check statechange */
#define EC_TIMEOUTSTATE (2000000)

/** default number of retries if wkc <= 0 */
#define EC_DEFAULTRETRIES (3)

/* MAC addresses */

/** Primary source MAC address used for EtherCAT. */
#define EC_PRIMARY_MAC_ARRAY {0x01,0x01,0x01,0x01,0x01,0x01}

/** Secondary source MAC address used for EtherCAT. */
#define EC_SECONDARY_MAC_ARRAY {0x04,0x04,0x04,0x04,0x04,0x04}

#ifdef __cplusplus
}
#endif

#endif
