"""A Starknet node for re-proving a capture's transaction at its block, with a newer prover.

Answers JSON-RPC from a capture's rpc-log.jsonl: a recorded call with the same method and params
gets the recorded result, a block id counting as the recording's block whether it is "latest",
its number or its hash. Classes, which the captures keep only the head of, come from the live
NODE (they never change), and are logged trimmed again. What else the recording lacks goes to the
NODE too, at the recording's block (never "latest"), and is reported as a miss. Every call served is appended to OUT_LOG in
rpc-log.jsonl's format, so OUT_LOG is the new capture's recording: what the newer prover read.

Env: CAPTURE (a capture directory), OUT_LOG, NODE (default http://127.0.0.1:9545/rpc/v0_10),
PORT (default 9548).
"""
import http.server, json, os, sys, threading, urllib.request

CAPTURE = os.environ["CAPTURE"]
OUT_LOG = os.environ["OUT_LOG"]
NODE = os.environ.get("NODE", "http://127.0.0.1:9545/rpc/v0_10")
PORT = int(os.environ.get("PORT", "9548"))

recorded = [json.loads(line) for line in open(os.path.join(CAPTURE, "rpc-log.jsonl"))]
header = next(e["response"]["result"] for e in recorded if e["request"]["method"] == "starknet_getBlockWithTxHashes")
BLOCK_NUMBER, BLOCK_HASH = header["block_number"], header["block_hash"]
lock = threading.Lock()


def same_block(block_id):
    return block_id == "latest" or block_id == {"block_number": BLOCK_NUMBER} or block_id == {"block_hash": BLOCK_HASH}


def normalized(params):
    """The params with the recording's block, however named, as one value."""
    if isinstance(params, dict) and "block_id" in params and same_block(params["block_id"]):
        return {**params, "block_id": "RECORDED"}
    return params


def proof_sets(params):
    """A storage proof request as sets: a prover lists its classes, contracts and keys in hash-set
    order, which differs from run to run, while the answer is the same set of trie nodes."""
    return (
        normalized({"block_id": params.get("block_id")}),
        sorted(params.get("class_hashes", [])),
        sorted(params.get("contract_addresses", [])),
        sorted((k["contract_address"], sorted(k["storage_keys"])) for k in params.get("contracts_storage_keys", [])),
    )


def reordered_proof(recorded_params, params, response):
    """The recorded storage proof with its per-contract lists in the order `params` asks for."""
    result = json.loads(json.dumps(response["result"]))
    leaves = dict(zip(recorded_params["contract_addresses"], result["contracts_proof"]["contract_leaves_data"]))
    result["contracts_proof"]["contract_leaves_data"] = [leaves[a] for a in params["contract_addresses"]]
    recorded_order = [k["contract_address"] for k in recorded_params["contracts_storage_keys"]]
    storage = dict(zip(recorded_order, result["contracts_storage_proofs"]))
    result["contracts_storage_proofs"] = [storage[k["contract_address"]] for k in params["contracts_storage_keys"]]
    return {**response, "result": result}


def find(method, params):
    for entry in recorded:
        if entry["request"]["method"] != method:
            continue
        if method == "starknet_getStorageProof" and "result" in entry["response"]:
            if proof_sets(entry["request"]["params"]) == proof_sets(params):
                return reordered_proof(entry["request"]["params"], params, entry["response"])
        elif normalized(entry["request"]["params"]) == normalized(params):
            return entry["response"]
    return None


def at_recorded_block(params):
    if isinstance(params, dict) and "block_id" in params and same_block(params["block_id"]):
        return {**params, "block_id": {"block_number": BLOCK_NUMBER}}
    return params


def from_node(request):
    body = json.dumps({**request, "params": at_recorded_block(request.get("params"))}).encode()
    upstream = urllib.request.Request(NODE, data=body, headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(upstream, timeout=600) as answer:
            return json.loads(answer.read())
    except urllib.error.HTTPError as error:
        return json.loads(error.read())


def trimmed(request, response):
    """A class as the captures keep it: the head of its Sierra program, all a replay reads."""
    if request["method"] == "starknet_getClass" and "result" in response:
        return {**response, "result": {"sierra_program": response["result"]["sierra_program"][:3]}}
    return response


def answer(request):
    # The captures keep only a class's head, so classes (which never change) come from the node.
    response = None if request["method"] == "starknet_getClass" else find(request["method"], request.get("params"))
    source = "recorded"
    if request["method"] == "starknet_getClass":
        response, source = from_node(request), "node (class)"
    if response is None:
        response = from_node(request)
        source = "node"
        print(f"miss: {request['method']} {json.dumps(request.get('params'))[:300]} -> "
              f"{'error ' + json.dumps(response.get('error'))[:200] if 'error' in response else 'ok'}", flush=True)
    response = {**response, "id": request.get("id")}
    with lock, open(OUT_LOG, "a") as log:
        log.write(json.dumps({"request": request, "response": trimmed(request, response), "source": source}) + "\n")
    return response


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
        reply = [answer(r) for r in body] if isinstance(body, list) else answer(body)
        data = json.dumps(reply).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass


print(f"recording of block {BLOCK_NUMBER} on :{PORT}", flush=True)
sys.stdout.flush()
http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
