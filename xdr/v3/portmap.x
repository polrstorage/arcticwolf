/* Portmapper Protocol v2 (RFC 1833) */
/* Program number: 100000 */

/* ===== Constants ===== */

const PMAP_PORT = 111;         /* Well-known portmapper port */
const PMAP_PROGRAM = 100000;   /* Portmapper program number */
const PMAP_VERSION = 2;        /* Portmapper version 2 */

/* ===== Portmapper Types ===== */

/* Protocol types */
const IPPROTO_TCP = 6;
const IPPROTO_UDP = 17;

/* Mapping of a program number to its network address */
struct mapping {
    unsigned int prog;      /* Program number */
    unsigned int vers;      /* Version number */
    unsigned int prot;      /* Protocol (IPPROTO_TCP or IPPROTO_UDP) */
    unsigned int port;      /* Port number */
};

/* Result of PMAPPROC_GETPORT */
struct pmaplist {
    mapping map;
    pmaplist *next;
};

/* Boolean result */
typedef bool bool_result;

/* Port number result */
typedef unsigned int port_result;

/* ===== Portmapper Procedures ===== */

/* PMAPPROC_NULL (0)
 * Arguments: void
 * Results: void
 * Purpose: Test connectivity
 */

/* PMAPPROC_SET (1)
 * Arguments: mapping
 * Results: bool (true if successfully registered)
 * Purpose: Register a service
 */

/* PMAPPROC_UNSET (2)
 * Arguments: mapping
 * Results: bool (true if successfully unregistered)
 * Purpose: Unregister a service
 */

/* PMAPPROC_GETPORT (3)
 * Arguments: mapping (only prog, vers, prot are used; port is ignored)
 * Results: unsigned int (port number, 0 if not found)
 * Purpose: Query the port for a service
 */

/* PMAPPROC_DUMP (4)
 * Arguments: void
 * Results: pmaplist (list of all registered services)
 * Purpose: Get list of all registered services
 */

/* PMAPPROC_CALLIT (5) - Not implemented */
