/* RFC 5531 - RPC: Remote Procedure Call Protocol Specification Version 2 */
/* Simplified version for fastxdr compatibility */

/* RPC Message Types */
enum msg_type {
    CALL  = 0,
    REPLY = 1
};

/* RPC Version */
const RPC_VERSION = 2;

/* Authentication flavor */
enum auth_flavor {
    AUTH_NONE  = 0,
    AUTH_SYS   = 1,
    AUTH_SHORT = 2,
    AUTH_DH    = 3,
    RPCSEC_GSS = 6
};

/* Authentication data */
struct opaque_auth {
    auth_flavor flavor;
    opaque body<400>;
};

/* AUTH_SYS credentials (RFC 5531 Appendix A) */
struct auth_sys_params {
    unsigned int stamp;
    string machinename<255>;
    unsigned int uid;
    unsigned int gid;
    unsigned int gids<16>;
};

/* Version mismatch info */
struct mismatch_info {
    unsigned int low;
    unsigned int high;
};

/* Accept status */
enum accept_stat {
    SUCCESS       = 0,
    PROG_UNAVAIL  = 1,
    PROG_MISMATCH = 2,
    PROC_UNAVAIL  = 3,
    GARBAGE_ARGS  = 4,
    SYSTEM_ERR    = 5
};

/* Reject status */
enum reject_stat {
    RPC_MISMATCH = 0,
    AUTH_ERROR   = 1
};

/* Auth error status */
enum auth_stat {
    AUTH_OK           = 0,
    AUTH_BADCRED      = 1,
    AUTH_REJECTEDCRED = 2,
    AUTH_BADVERF      = 3,
    AUTH_REJECTEDVERF = 4,
    AUTH_TOOWEAK      = 5,
    AUTH_INVALIDRESP  = 6,
    AUTH_FAILED       = 7
};

/* Reply status */
enum reply_stat {
    MSG_ACCEPTED = 0,
    MSG_DENIED   = 1
};

/* RPC Call Body */
struct call_body {
    unsigned int rpcvers;
    unsigned int prog;
    unsigned int vers;
    unsigned int proc;
    opaque_auth cred;
    opaque_auth verf;
};

/* ===== RPC Call Message ===== */

struct rpc_call_msg {
    unsigned int xid;
    msg_type mtype;          /* Must be CALL (0) */
    unsigned int rpcvers;    /* Must be 2 */
    unsigned int prog;
    unsigned int vers;
    unsigned int proc;
    opaque_auth cred;
    opaque_auth verf;
};

/* ===== RPC Reply Messages ===== */

/* Accepted reply for successful calls */
struct accepted_reply_success {
    opaque_auth verf;
    opaque result_data<>;
};

/* Accepted reply for program mismatch */
struct accepted_reply_mismatch {
    opaque_auth verf;
    mismatch_info mismatch_info;
};

/* Accepted reply for other errors (no additional data) */
struct accepted_reply_error {
    opaque_auth verf;
};

/* Rejected reply for RPC version mismatch */
struct rejected_reply_mismatch {
    mismatch_info mismatch_info;
};

/* Rejected reply for auth error */
struct rejected_reply_auth {
    auth_stat auth_stat;
};

/* ===== Complete RPC Reply Message ===== */
/*
 * This is a flattened version since fastxdr has limited union support.
 * In practice, you need to interpret the message based on reply_stat and accept_stat.
 */
struct rpc_reply_msg {
    unsigned int xid;
    msg_type mtype;            /* Must be REPLY (1) */
    reply_stat stat;           /* MSG_ACCEPTED or MSG_DENIED */
    opaque_auth verf;          /* Verifier (for MSG_ACCEPTED) */
    accept_stat accept_stat;   /* Accept status (for MSG_ACCEPTED with SUCCESS) */
};
