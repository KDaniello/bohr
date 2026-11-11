package bohr

import "runtime"

// The records a store's harness writes for the laws of durability: what it began, what the store
// acknowledged, what it read back, and when it first answered after a start (docs/API.md §6.5).
//
// The harness judges nothing; the world compares. Each acknowledgement names the line it was made
// on, so a finding is placed at it.

// KvOp is what a store was asked to do: "put", "delete", or "batch".
type KvOp string

const (
	KvPut    KvOp = "put"
	KvDelete KvOp = "delete"
	KvBatch  KvOp = "batch"
)

func kvOp(stage string, op KvOp, keys []string, values []*string, seq uint64) {
	vals := make([]any, len(keys))
	for i := range keys {
		if i < len(values) && values[i] != nil {
			vals[i] = *values[i]
		}
	}
	details := map[string]any{"op": string(op), "keys": keys, "values": vals, "seq": seq}
	if _, file, line, ok := runtime.Caller(2); ok {
		details["at"] = map[string]any{"file": file, "line": line}
	}
	SendEvent(stage, details)
}

// KvBegun records an operation begun, which may land or not. A nil value removes its key.
func KvBegun(op KvOp, keys []string, values []*string, seq uint64) {
	kvOp("kv.begun", op, keys, values, seq)
}

// KvAcked records an operation the store acknowledged.
func KvAcked(op KvOp, keys []string, values []*string, seq uint64) {
	kvOp("kv.acked", op, keys, values, seq)
}

// KvRead records a key read back: its value, or nil where it is absent.
func KvRead(key string, value *string) {
	var v any
	if value != nil {
		v = *value
	}
	SendEvent("kv.read", map[string]any{"key": key, "value": v})
}

// KvReady records that the store answered its first question since this start.
func KvReady() {
	SendEvent("kv.ready", map[string]any{})
}
