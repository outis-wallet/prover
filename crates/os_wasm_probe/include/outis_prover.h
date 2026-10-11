/* The Outis prover's C interface for proving a live transaction natively (src/native.rs): a
 * session per transaction, driven method by method with JSON, as js/tx-driver.mjs drives the
 * wasm TxSession. Link the static library built by scripts/build-android.sh (or the iOS one). */
#ifndef OUTIS_PROVER_H
#define OUTIS_PROVER_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct TxNativeSession TxNativeSession;

/* Opens a session for one transaction on a rayon pool of `threads` threads (0: one per core),
   16 MiB stacks (see replay.rs for why). On failure returns NULL and, if `error` is not NULL,
   sets *error to a message to free with tx_prove_free (on success, sets *error to NULL). */
TxNativeSession *tx_session_open(const char *chain_id, const char *header_json,
                                 const char *rpc_transaction_json, uint32_t threads, char **error);

/* Calls one method. `method`: tx_hash, block_number, set_placeholders, add_reads, add_class,
   add_undeclared_class, execute, set_storage_proofs, run_os, prove, verify.
   `args_json`: a JSON array of the arguments in the wasm method's order (strings; a bool for
   set_placeholders; verify takes the prove result JSON string); NULL for no arguments.
   Returns {"ok": value} (a string as the wasm method returns it, a number for block_number, or null)
   or {"error": "message"} or {"error": "message", "cancelled": true}. Free with tx_prove_free.
   set_storage_proofs returns {"ok": null} natively: the OS input stays in the session (it is
   megabytes and nothing live reads it). Runs the method inside the session's pool. Calls from
   several threads take turns (each holds the session for its whole run). After a call that
   panicked, every later call returns an error: close the session. */
char *tx_session_call(TxNativeSession *session, const char *method, const char *args_json);

/* Asks the session to stop: a call in progress returns {"cancelled": true, ...} at its next
   checkpoint, and every later call at once. Safe from any thread while a call runs (it never
   waits), but not once tx_session_close has been called. */
void tx_session_cancel(TxNativeSession *session);

/* Frees the session. Must not race a running call: if one is still running, this blocks until it
   returns (cancel first to make that quick). No call or cancel may start once this is called. */
void tx_session_close(TxNativeSession *session);

/* Frees a string returned by this library (tx_session_call, tx_session_open's error,
   tx_prove_replay). */
void tx_prove_free(char *answer);

#ifdef __cplusplus
}
#endif

#endif /* OUTIS_PROVER_H */
