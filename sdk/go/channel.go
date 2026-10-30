package bohr

import (
	"encoding/json"
	"os"
	"strconv"
	"strings"
	"sync"
)

// Schema is the record format this package writes. See docs/API.md §6.3.
const Schema = 1

// Where the supervisor says to write, in the order the variables are consulted. Ours first, then
// the Antithesis name, so a program already instrumented for their platform runs here unchanged.
const (
	outputVar     = "BOHR_SDK_OUTPUT"
	antithesisVar = "ANTITHESIS_SDK_LOCAL_OUTPUT"
	nodeVar       = "BOHR_SDK_NODE"
	armedVar      = "BOHR_ARMED_FAULT_POINTS"
)

// The open output, its sequence counter and what this run armed.
type channel struct {
	mu    sync.Mutex
	out   *os.File
	seq   uint64
	node  int
	armed map[string]struct{}
}

var (
	resolveOnce sync.Once
	current     *channel
)

// open resolves the channel the first time anything asks for it, and answers from that afterwards.
//
// Nil means nothing is listening, which is the ordinary case: this package is compiled into a
// program that spends most of its life outside Bohr, and there it must cost nothing at all.
func open() *channel {
	resolveOnce.Do(resolve)
	return current
}

func resolve() {
	target := os.Getenv(outputVar)
	if target == "" {
		target = os.Getenv(antithesisVar)
	}
	if target == "" {
		return
	}

	var out *os.File
	if fd, err := strconv.Atoi(target); err == nil {
		// A bare number names a descriptor the supervisor has already opened, which is how it
		// hands one over without a path existing anywhere the guest can see. Adopted directly:
		// Go can take a descriptor as a file, where Rust has to go through /proc to stay free of
		// unsafe code.
		//
		// Then asked whether it is anything. Adopting a descriptor does not check that one is
		// open, so a variable holding a number that means nothing would otherwise produce a
		// channel that silently swallows every record — where the other SDKs, which have to open
		// something to adopt it, are simply left with no channel at all.
		adopted := os.NewFile(uintptr(fd), "bohr-sdk-output")
		if adopted == nil {
			return
		}
		if _, err := adopted.Stat(); err != nil {
			return
		}
		out = adopted
	} else {
		opened, err := os.OpenFile(target, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o600)
		if err != nil {
			return
		}
		out = opened
	}

	armed := map[string]struct{}{}
	for _, name := range strings.Split(os.Getenv(armedVar), ",") {
		if name != "" {
			armed[name] = struct{}{}
		}
	}
	node, _ := strconv.Atoi(os.Getenv(nodeVar))
	current = &channel{out: out, node: node, armed: armed}
}

// emit writes one record, if anything is listening.
func emit(key string, body any) {
	c := open()
	if c == nil {
		return
	}
	c.mu.Lock()
	defer c.mu.Unlock()

	// Under the same lock as the write, so the sequence numbers and the line order agree. The
	// supervisor reads a gap as a record it never saw — a guest writing through a path it did not
	// intercept — and a counter incremented outside the lock would manufacture that alarm.
	c.seq++
	line, err := encode(key, body, c.seq, c.node)
	if err != nil {
		return
	}
	// A record that cannot be written is dropped rather than raised: this package is a bystander
	// in somebody else's program, and it is in no position to end one.
	_, _ = c.out.Write(line)
}

// encode builds one line of the record channel.
//
// Maps rather than structs throughout: Go sorts map keys when it encodes them, which reproduces
// the ordering the other SDKs get from their own ordered maps without anyone having to keep a
// struct's fields alphabetical.
func encode(key string, body any, seq uint64, node int) ([]byte, error) {
	record := map[string]any{
		key: body,
		"bohr": map[string]any{
			"schema": Schema,
			"seq":    seq,
			"node":   node,
		},
	}

	var out strings.Builder
	encoder := json.NewEncoder(&out)
	// Off, or this SDK would be the one whose records spell `<`, `>` and `&` differently from
	// every other. The conformance suite parses both sides and would not notice.
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(record); err != nil {
		return nil, err
	}
	// Encode appends the newline the channel wants, so nothing is added here.
	return []byte(out.String()), nil
}
